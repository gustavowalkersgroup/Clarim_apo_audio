//! `AppBackend`: the `clarim_core::Backend` behind the dashboard.
//!
//! Owns the config, the mic engine and the routing engine and keeps them in step: every `set_*`
//! validates its input, applies it live and saves the config. A housekeeping thread folds in
//! calibration results, retries a dead mic and refreshes the device lists.
//!
//! `--dry-run` / `--no-routing` are runtime overrides, never saved: the snapshot reports what is
//! really in force (mic off, routing off), and changes that would break the override are refused.
//!
//! Lock order: `mic` before `cfg`; `eq_order` before `cfg`; `eq_refresh` before everything it reads
//! (`devices`, `eq`). `devices`, `last_mic`, `eq`, `routing`, `call` and `housekeeper` are leaf locks (`routing` is
//! also taken under `cfg`, only for a non-blocking send, so updates reach it in order).
//!
//! The call-aware part of the 1 s housekeeping tick (`watch_call`, design: `docs/leak-agc.md` section 6.2) runs
//! under `mic`: it never joins a thread or waits on COM for long, and it publishes what the dashboard shows
//! through the `call` leaf lock.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use clarim_audio::devices::is_denied;
use clarim_audio::{
    default_render_id, endpoint_volume, set_endpoint_volume_db, virtual_mic_capture, EndpointVolume, MicEngine,
};
use clarim_core::{
    endpoint_guid, AppSession, Backend, CaptureSession, ChannelId, ChannelSettings, Config, DeviceInfo, EqApoStatus,
    LeakKind, Meters, MicSettings, MicStatus, MicVolumeTaken, OutputEq, Snapshot, EQ_TONE_RANGE_DB,
};
use clarim_routing::{RoutingConfig, RoutingEngine, UNKNOWN_EXE};
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::callwatch::{CallTracker, GuardOff, LeakMeasures, LeakMonitor, MicVolumeGuard, VolAction};
use crate::eqapo::{self, Service};
use crate::tray::Balloon;
use crate::{tray, AgentFlags};

/// Writes the config to disk. A plain function so tests can keep it off the real `%APPDATA%`.
pub type SaveFn = fn(&Config) -> Result<()>;

/// Writes every saved output EQ to Equalizer APO. A plain function so tests never touch Program Files.
pub type EqWriteFn = fn(&BTreeMap<String, OutputEq>) -> Result<()>;

const TICK: Duration = Duration::from_secs(1);
const RESCAN_EVERY: Duration = Duration::from_secs(5);
const RETRY_FIRST: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How often the Equalizer APO state is looked at again (and a failed write retried). While the Clarim
/// service is installing or restarting, every housekeeping tick (1 s) looks, so the dashboard follows it.
const EQ_APO_EVERY: Duration = Duration::from_secs(10);
/// While in a call, the tuning line (echo score, levels, AGC gain...) goes to the log this often.
const CALL_LOG_EVERY: Duration = Duration::from_secs(10);
/// How long the note about a Windows mic volume change shows on the dashboard.
const VOLUME_NOTE_TTL: Duration = Duration::from_secs(60);
/// Loopback references running at once: `clarim_audio`'s `MAX_REFS` (the engine drops the rest, but logs it).
const MAX_REFERENCES: usize = 4;
/// A mic whose Windows volume spans less than this has no room for one lowering step.
const MIN_VOLUME_RANGE_DB: f32 = 3.0;
/// A read-back volume change under this counts as "nothing moved" (same as the guard's); a volume this close
/// to the baseline counts as back at it.
const VOLUME_EPSILON_DB: f32 = 0.5;

const TIP_IDLE: &str = "Clarim";
const TIP_CALL: &str = "Clarim · em chamada";
const TIP_LEAK: &str = "Clarim · som vazando na chamada";

const MSG_SATURATING: &str = "O microfone está saturando antes do volume do Windows: diminua o 'Aumento do microfone' (Propriedades do microfone > Níveis) ou afaste o microfone.";
const BALLOON_LOWERED: (&str, &str) = ("Microfone estourando", "Baixei o volume do microfone no Windows em 3 dB.");

const MSG_DRY_RUN_MIC: &str = "Modo de teste (--dry-run): o microfone fica desligado.";
const MSG_OBSERVE_ONLY: &str = "Modo de teste (--dry-run / --no-routing): o roteamento fica só em observação.";

/// Every active endpoint; the deny-list is applied only when handing lists to the dashboard.
#[derive(Default)]
pub struct Scan {
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

/// Enumerates every active endpoint. The calling thread gets COM (MTA) first (best effort: an
/// STA thread can enumerate too).
pub fn scan_devices() -> Result<Scan> {
    let _ = clarim_audio::init_com();
    Ok(Scan { inputs: clarim_audio::list_inputs(&[])?, outputs: clarim_audio::list_outputs(&[])? })
}

/// Digital outputs (S/PDIF, HDMI...) usually have nothing plugged in. Routing every app to one would
/// silence the machine, so they are only picked when nothing else is left.
fn is_digital_output(name: &str) -> bool {
    const MARKS: [&str; 6] = ["s/pdif", "spdif", "hdmi", "digital", "optical", "displayport"];
    let n = name.to_lowercase();
    MARKS.iter().any(|m| n.contains(m))
}

/// First run only. Two things:
/// - SteelSeries Sonar installed (any playback device named like it): routing starts OFF, so a first
///   launch never fights Sonar over every app's output. The user turns it on from the dashboard.
/// - When the Windows default playback device is virtual (a Sonar channel, say), "Windows default"
///   would just feed that virtual device, so point every channel at the first real, non-denied
///   playback device (analog ones first). Otherwise channels stay on the Windows default.
pub fn first_run_defaults(cfg: &mut Config, outputs: &[DeviceInfo]) {
    if outputs.iter().any(|d| d.name.to_lowercase().contains("sonar")) {
        cfg.routing_enabled = false;
    }
    if !outputs.iter().any(|d| d.is_default && d.is_virtual) {
        return;
    }
    let mut real = outputs.iter().filter(|d| !d.is_virtual && !is_denied(&d.name, &cfg.deny_devices));
    let pick = real.clone().find(|d| !is_digital_output(&d.name)).or_else(|| real.next());
    if let Some(device) = pick {
        for ch in cfg.channels.values_mut() {
            ch.device_id = Some(device.id.clone());
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn visible(devs: &[DeviceInfo], deny: &[String]) -> Vec<DeviceInfo> {
    devs.iter().filter(|d| !is_denied(&d.name, deny)).cloned().collect()
}

fn routing_config(cfg: &Config, observe_only: bool) -> RoutingConfig {
    RoutingConfig {
        channels: cfg.channels.clone(),
        app_rules: cfg.app_rules.clone(),
        default_channel: cfg.default_channel,
        enabled: cfg.routing_enabled && !observe_only,
        call_to_chat: cfg.call_to_chat,
    }
}

/// What the mic engine is told: under `--dry-run` it is always disabled.
fn engine_settings(s: &MicSettings, dry_run: bool) -> MicSettings {
    MicSettings { enabled: s.enabled && !dry_run, ..s.clone() }
}

/// An empty id from the dashboard means "automatic".
fn clean_id(id: Option<String>) -> Option<String> {
    id.filter(|s| !s.trim().is_empty())
}

/// Range-checks incoming mic settings. Out-of-range numbers are clamped, NaN/inf is refused.
/// `voice_level_db` is kept from `stored`: only a calibration writes it, so a stale copy posted by
/// the dashboard can never undo a fresh calibration. `mic_volume_taken` is kept the same way: it is
/// Clarim's own bookkeeping of the Windows volume it lowered.
fn check_mic(mut s: MicSettings, stored: &MicSettings, dry_run: bool) -> Result<MicSettings> {
    for (v, lo, hi, what) in [
        (&mut s.denoise_strength, 0.0, 1.0, "intensidade da supressão de ruído"),
        (&mut s.voice_focus_db, -40.0, 0.0, "sensibilidade do foco na voz"),
        (&mut s.gate_threshold_db, -90.0, 0.0, "limiar do noise gate"),
        (&mut s.input_gain_db, -20.0, 20.0, "ganho de entrada"),
        (&mut s.output_gain_db, -20.0, 20.0, "ganho de saída"),
        (&mut s.eq_bass_db, -EQ_TONE_RANGE_DB, EQ_TONE_RANGE_DB, "graves do equalizador"),
        (&mut s.eq_mid_db, -EQ_TONE_RANGE_DB, EQ_TONE_RANGE_DB, "médios do equalizador"),
        (&mut s.eq_treble_db, -EQ_TONE_RANGE_DB, EQ_TONE_RANGE_DB, "agudos do equalizador"),
    ] {
        if !v.is_finite() {
            bail!("Valor inválido para {what}.");
        }
        *v = v.clamp(lo, hi);
    }
    s.voice_level_db = stored.voice_level_db;
    s.mic_volume_taken = stored.mic_volume_taken.clone();
    s.input_id = clean_id(s.input_id);
    s.output_id = clean_id(s.output_id);
    s.monitor_id = clean_id(s.monitor_id);
    if dry_run {
        if s.enabled {
            bail!(MSG_DRY_RUN_MIC);
        }
        s.enabled = stored.enabled; // the dashboard saw the override (off), not the saved value
    }
    Ok(s)
}

/// Most per-app rules kept in the config; a new key beyond this is refused (existing keys stay editable).
const MAX_APP_RULES: usize = 512;

/// Most output-EQ devices kept in the config (a PC has a handful of playback devices); same rule as above.
const MAX_OUTPUT_EQ: usize = 64;

/// Lowercase exe file name; refuses anything that is not a plain Windows file name: path
/// separators, control characters and the characters Windows forbids in file names.
fn normalize_exe(exe: &str) -> Result<String> {
    let exe = exe.trim().to_lowercase();
    let bad_char = |c: char| c.is_control() || matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*');
    if exe.is_empty() || exe.len() > 260 || exe.contains(bad_char) {
        bail!("Nome de programa inválido.");
    }
    Ok(exe)
}

/// Retry pacing for a mic that is enabled but not running: 3 s, doubling up to 30 s so a missing
/// virtual cable does not fill the log.
struct Retry {
    delay: Duration,
    next: Instant,
}

impl Retry {
    fn new() -> Self {
        Self { delay: RETRY_FIRST, next: Instant::now() + RETRY_FIRST }
    }

    fn due(&self) -> bool {
        Instant::now() >= self.next
    }

    /// `reconnecting` = the engine says Windows Audio is restarting (the user pressed "Reiniciar áudio"): it is
    /// back in seconds, so keep knocking every `RETRY_FIRST` instead of backing off.
    fn bump(&mut self, reconnecting: bool) {
        if reconnecting {
            self.delay = RETRY_FIRST;
        }
        self.next = Instant::now() + self.delay;
        if !reconnecting {
            self.delay = (self.delay * 2).min(RETRY_MAX);
        }
    }
}

/// What the dashboard is told about Equalizer APO. Refreshed by `refresh_eq_apo`, never per snapshot.
#[derive(Default)]
struct EqState {
    status: EqApoStatus,
    /// PT-BR text of the last failed write; cleared by the next successful one.
    write_error: Option<String>,
    /// The saved equalizers are not in the APO file yet (not written since start, or the last write
    /// failed): housekeeping writes them once Equalizer APO is installed and somebody can write.
    stale: bool,
}

/// What the dashboard is told about the call: the overlay fields of `MicStatus` (the engine leaves them at
/// their defaults). Written by the housekeeping tick, read by every status; a leaf lock.
#[derive(Default)]
struct CallOverlay {
    in_call: bool,
    apps: Vec<String>,
    leak: Option<LeakKind>,
    note: Option<String>,
    too_quiet: bool,
}

impl CallOverlay {
    fn apply_to(&self, st: &mut MicStatus) {
        st.in_call = self.in_call;
        st.call_apps.clone_from(&self.apps);
        st.leak = self.leak;
        st.mic_volume_note.clone_from(&self.note);
        st.mic_too_quiet = self.too_quiet;
    }
}

/// A PT-BR note about the last Windows mic volume action; `until` is `None` for one that stays for the run.
struct Note {
    text: String,
    until: Option<Instant>,
}

/// The Windows mic volume as this agent reaches it (read, write: both return what the device really holds);
/// fakes in tests, so nothing touches a real microphone.
#[derive(Clone, Copy)]
struct MicVolume {
    read: fn(&str) -> Result<EndpointVolume>,
    write: fn(&str, f32) -> Result<f32>,
}

impl MicVolume {
    const REAL: Self = Self { read: endpoint_volume, write: set_endpoint_volume_db };
}

/// Everything the call-aware part of the housekeeping tick remembers between ticks. Owned by the housekeeping
/// thread (only `watch_call` touches it); what the dashboard reads goes through `CallOverlay`.
struct Watch {
    tracker: CallTracker,
    monitor: LeakMonitor,
    guard: MicVolumeGuard,
    /// `MicStatus::clip_events` of the previous tick.
    prev_clips: u32,
    in_call: bool,
    /// The tooltip on show.
    tip: &'static str,
    /// Per call: echo detection was reported as off.
    refs_off_logged: bool,
    /// Per call: the "Microfone estourando" balloon is waiting for the user to be free / has been shown.
    lowered_balloon_due: bool,
    lowered_balloon_told: bool,
    note: Option<Note>,
    last_call_log: Option<Instant>,
}

impl Watch {
    /// `taken_db`: what the persisted `MicVolumeTaken` record says Clarim took (0 without one).
    fn new(taken_db: f32) -> Self {
        Self {
            tracker: CallTracker::default(),
            monitor: LeakMonitor::default(),
            guard: MicVolumeGuard::new(taken_db),
            prev_clips: 0,
            in_call: false,
            tip: TIP_IDLE,
            refs_off_logged: false,
            lowered_balloon_due: false,
            lowered_balloon_told: false,
            note: None,
            last_call_log: None,
        }
    }

    /// Logs the start and end of a call, and forgets the per-call memory once it is over.
    fn track_call(&mut self, in_call: bool, apps: &[String]) {
        if in_call != self.in_call {
            self.in_call = in_call;
            if in_call {
                log::info!("call started ({})", if apps.is_empty() { "unknown app".to_owned() } else { apps.join(", ") });
            } else {
                log::info!("call ended");
            }
        }
        if !in_call {
            self.refs_off_logged = false;
            self.lowered_balloon_due = false;
            self.lowered_balloon_told = false;
            self.last_call_log = None;
        }
    }

    fn note_for_a_minute(&mut self, text: String, now: Instant) {
        self.note = Some(Note { text, until: Some(now + VOLUME_NOTE_TTL) });
    }

    fn note_for_the_run(&mut self, text: &str) {
        self.note = Some(Note { text: text.to_owned(), until: None });
    }

    /// The note to show now; one past its time is dropped.
    fn note_text(&mut self, now: Instant) -> Option<String> {
        if self.note.as_ref().is_some_and(|n| n.until.is_some_and(|t| now >= t)) {
            self.note = None;
        }
        self.note.as_ref().map(|n| n.text.clone())
    }
}

/// The render endpoints to listen to for echo: the devices the four channels play on (a channel on the Windows
/// default plays on `default_console`), plus both Windows defaults, in that order, without repeats and at most
/// [`MAX_REFERENCES`]. Left out: anything that is not a listed real output, and the ids in `own` (the virtual
/// mic's own render side, and where "Ouvir meu microfone" plays: both carry the user's own voice).
fn pick_references(
    channel_devices: &[Option<String>],
    default_console: Option<&str>,
    default_comms: Option<&str>,
    outputs: &[DeviceInfo],
    own: &[Option<String>],
) -> Vec<String> {
    let mut picked: Vec<String> = Vec::new();
    let candidates = channel_devices.iter().map(|d| d.as_deref().or(default_console)).chain([default_console, default_comms]);
    for id in candidates.flatten() {
        let is_own = own.iter().flatten().any(|o| o.eq_ignore_ascii_case(id));
        let listed = outputs.iter().find(|d| d.id.eq_ignore_ascii_case(id) && !d.is_virtual);
        if let Some(d) = listed.filter(|_| !is_own) {
            if !picked.contains(&d.id) {
                picked.push(d.id.clone());
            }
        }
    }
    picked.truncate(MAX_REFERENCES);
    picked
}

/// Does the mic volume guard run this tick? In a call, with the voice AGC and the setting on, the mic running,
/// and, when a record of a lowered volume is kept, that record being about the mic in use (a lowered volume
/// on some other mic is not this mic's business).
fn guard_active(mic: &MicSettings, in_call: bool, running: bool, input_id: Option<&str>) -> bool {
    let record_fits = mic.mic_volume_taken.as_ref().is_none_or(|r| input_id == Some(r.device_id.as_str()));
    in_call && running && input_id.is_some() && mic.auto_level && mic.auto_mic_volume && record_fits
}

/// Clipped speech frames since the last tick. `MicStatus::clip_events` restarts at 0 with the engine, so a
/// lower count means a new engine whose clipped frames are all new.
fn clip_delta(prev: u32, now: u32) -> u32 {
    if now >= prev {
        now - prev
    } else {
        now
    }
}

/// The tray tooltip: an alert beats a plain call.
fn tip_for(in_call: bool, alert: Option<LeakKind>) -> &'static str {
    match (in_call, alert) {
        (true, Some(_)) => TIP_LEAK,
        (true, None) => TIP_CALL,
        (false, _) => TIP_IDLE,
    }
}

/// Title and text of the balloon for a leak alert.
fn leak_balloon(kind: LeakKind) -> (&'static str, &'static str) {
    match kind {
        LeakKind::Echo => ("Som vazando na chamada", "O som das caixas está entrando no seu microfone. Abaixe o volume ou use fone."),
        LeakKind::Noise => {
            ("Ruído passando na chamada", "Tem ruído de fundo passando pelo filtro. Aproxime o microfone ou ligue o Foco na voz.")
        }
    }
}

/// The 10 s tuning line of a call, for calibrating the thresholds from `clarim.log`: echo score, lag, leak
/// level, speech level at the cable, AGC gain, residual noise and the number of references.
fn log_call_line(st: &MicStatus) {
    let db = |v: Option<f32>| v.map_or_else(|| "-".to_owned(), |d| format!("{d:.1}"));
    log::info!(
        "call: echo score {:.2} lag {:.0} ms level {} dBFS | voice {} dBFS agc {:+.1} dB noise {} dBFS | refs {}",
        st.echo_score,
        st.echo_lag_ms,
        db(st.echo_db),
        db(st.voice_db),
        st.agc_gain_db,
        db(st.noise_db),
        st.references
    );
}

/// "3" for 3.0, "1,5" for 1.5 (PT-BR decimal comma).
fn fmt_db(db: f32) -> String {
    let s = format!("{db:.1}");
    s.strip_suffix(".0").unwrap_or(&s).replace('.', ",")
}

/// What a lowering or giving back comes to once Windows has said what the volume is now.
#[derive(Debug, PartialEq)]
enum VolPlan {
    Skip,
    /// Windows volume cannot help on this mic (no hardware control, no range to move in, or already at the floor).
    NoHardware,
    /// There is nothing (or no longer anything) to give back: the user put the volume back, or the record is gone.
    Forget,
    /// Set the volume to `target`; `record` is the bookkeeping to update afterwards.
    Set { target: f32, record: MicVolumeTaken },
}

fn plan_volume(action: VolAction, v: &EndpointVolume, record: Option<&MicVolumeTaken>, device_id: &str) -> VolPlan {
    match action {
        VolAction::None => VolPlan::Skip,
        VolAction::Lower(db) => {
            // Already at the floor counts as nothing left to lower: without this the guard would retry every
            // 10 s, rewriting config.json each time, and never tell the user.
            if !v.hardware || v.max_db - v.min_db < MIN_VOLUME_RANGE_DB || v.db - v.min_db < VOLUME_EPSILON_DB {
                return VolPlan::NoHardware;
            }
            let fresh = || MicVolumeTaken { device_id: device_id.to_owned(), baseline_db: v.db, taken_db: 0.0 };
            VolPlan::Set { target: (v.db - db).max(v.min_db), record: record.cloned().unwrap_or_else(fresh) }
        }
        VolAction::Raise(db) => match record {
            Some(r) if v.db < r.baseline_db - VOLUME_EPSILON_DB => {
                VolPlan::Set { target: (v.db + db).min(r.baseline_db), record: r.clone() }
            }
            _ => VolPlan::Forget,
        },
    }
}

pub struct AppBackend {
    flags: AgentFlags,
    save: SaveFn,
    eq_write: EqWriteFn,
    cfg: Mutex<Config>,
    devices: Mutex<Scan>,
    mic: Mutex<MicEngine>,
    /// Last status read (engine fields only), served while `apply` holds the engine.
    last_mic: Mutex<MicStatus>,
    /// The call state the dashboard sees, copied into every `MicStatus` this backend returns.
    call: Mutex<CallOverlay>,
    /// The real mic's Windows volume; fakes in tests.
    volume: MicVolume,
    /// `None` once shut down.
    routing: Mutex<Option<RoutingEngine>>,
    /// The Clarim service as this agent reaches it (the pipe); fakes in tests.
    svc: Service,
    /// Serializes "read the saved equalizers, write them", so an older write never lands after a newer one.
    eq_order: Mutex<()>,
    /// Serializes `refresh_eq_apo`, so a look that began before a service command never lands after the one
    /// that follows it (the dashboard would miss `busy` until the next look).
    eq_refresh: Mutex<()>,
    eq: Mutex<EqState>,
    stop: (Sender<()>, Receiver<()>),
    housekeeper: Mutex<Option<JoinHandle<()>>>,
}

impl AppBackend {
    /// Starts both engines: the mic (unless `--dry-run` or disabled in the config) and routing
    /// (observe-only unless enabled and not overridden).
    pub fn new(cfg: Config, flags: AgentFlags, devices: Scan, save: SaveFn) -> Self {
        let mut mic = MicEngine::new(cfg.deny_devices.clone());
        mic.apply(&engine_settings(&cfg.mic, flags.dry_run));
        let routing = RoutingEngine::start(routing_config(&cfg, flags.dry_run || flags.no_routing));
        Self {
            flags,
            save,
            eq_write: eqapo::apply,
            cfg: Mutex::new(cfg),
            devices: Mutex::new(devices),
            mic: Mutex::new(mic),
            last_mic: Mutex::new(MicStatus::default()),
            call: Mutex::new(CallOverlay::default()),
            volume: MicVolume::REAL,
            routing: Mutex::new(Some(routing)),
            svc: Service::REAL,
            eq_order: Mutex::new(()),
            eq_refresh: Mutex::new(()),
            eq: Mutex::new(EqState::default()),
            stop: bounded(1),
            housekeeper: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_eq_write(mut self, f: EqWriteFn) -> Self {
        self.eq_write = f;
        self
    }

    #[cfg(test)]
    fn with_service(mut self, svc: Service) -> Self {
        self.svc = svc;
        self
    }

    #[cfg(test)]
    fn with_volume(mut self, volume: MicVolume) -> Self {
        self.volume = volume;
        self
    }

    fn observe_only(&self) -> bool {
        self.flags.dry_run || self.flags.no_routing
    }

    /// The mic is off: paused in the config, or switched off by `--dry-run`. Drives the tray look.
    pub fn mic_paused(&self) -> bool {
        self.flags.dry_run || !lock(&self.cfg).mic.enabled
    }

    /// The config as it is really in force (overrides applied), for the dashboard.
    fn effective_config(&self) -> Config {
        let mut c = lock(&self.cfg).clone();
        if self.observe_only() {
            c.routing_enabled = false;
        }
        if self.flags.dry_run {
            c.mic.enabled = false;
        }
        c
    }

    fn persist(&self, cfg: &Config) -> Result<()> {
        (self.save)(cfg).map_err(|e| {
            log::warn!("cannot save the config: {e:#}");
            anyhow!("Não foi possível salvar a configuração: {e:#}")
        })
    }

    fn push_routing(&self, cfg: &Config) {
        if let Some(r) = lock(&self.routing).as_ref() {
            r.update(routing_config(cfg, self.observe_only()));
        }
    }

    /// Edits the config, applies it to routing and saves it, all under the config lock so concurrent
    /// edits land in order. `f` must validate before it mutates. A save failure is returned after the
    /// change is already live.
    fn edit(&self, f: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
        let mut cfg = lock(&self.cfg);
        f(&mut cfg)?;
        self.push_routing(&cfg);
        self.persist(&cfg)
    }

    fn sessions(&self) -> Vec<AppSession> {
        let all = lock(&self.routing).as_ref().map(RoutingEngine::sessions).unwrap_or_default();
        all.into_iter().filter(|s| s.exe != UNKNOWN_EXE).collect()
    }

    /// Mic status, with the call overlay on top. Never blocks on `apply`: while it holds the engine, the
    /// previous engine status is served (the overlay is always the current one).
    fn mic_status(&self) -> MicStatus {
        let mut st = match self.mic.try_lock() {
            Ok(mut eng) => self.read_engine_status(&mut eng),
            Err(TryLockError::Poisoned(p)) => self.read_engine_status(&mut p.into_inner()),
            Err(TryLockError::WouldBlock) => lock(&self.last_mic).clone(),
        };
        lock(&self.call).apply_to(&mut st);
        st
    }

    fn read_engine_status(&self, eng: &mut MicEngine) -> MicStatus {
        // Status first, then fold: whoever sees `calibrating == false` has the result already
        // saved (the engine publishes the result before it clears the flag).
        let st = eng.status();
        self.fold_calibration(eng);
        *lock(&self.last_mic) = st.clone();
        st
    }

    /// A finished calibration becomes `mic.voice_level_db`: saved, and pushed to the running engine.
    fn fold_calibration(&self, eng: &mut MicEngine) {
        let Some(db) = eng.take_calibration() else { return };
        let (settings, saved) = {
            let mut cfg = lock(&self.cfg);
            cfg.mic.voice_level_db = Some(db);
            (cfg.mic.clone(), self.persist(&cfg))
        };
        log::info!("voice calibration finished: {db:.1} dBFS{}", if saved.is_err() { " (not saved)" } else { "" });
        eng.apply(&engine_settings(&settings, self.flags.dry_run));
    }

    /// Re-enumerates the devices and keeps the old lists if that fails.
    fn rescan(&self) {
        match scan_devices() {
            Ok(s) => *lock(&self.devices) = s,
            Err(e) => log::debug!("device scan failed: {e:#}"),
        }
    }

    /// Every second: fold a calibration result, retry the mic if it should be running and is not, and watch the
    /// call (leaks, the real mic's Windows volume); every 5 s refresh the device lists; every 10 s look at
    /// Equalizer APO again.
    fn housekeep(&self) {
        let _ = clarim_audio::init_com();
        let (mut last_scan, mut last_apo) = (Instant::now(), Instant::now());
        let mut retry = Retry::new();
        let taken = lock(&self.cfg).mic.mic_volume_taken.as_ref().map_or(0.0, |r| r.taken_db);
        let mut watch = Watch::new(taken);
        self.write_stale_eq(); // the first write of the saved equalizers, off the start-up path
        while matches!(self.stop.1.recv_timeout(TICK), Err(crossbeam_channel::RecvTimeoutError::Timeout)) {
            self.tick_mic(&mut retry, &mut watch);
            if last_scan.elapsed() >= RESCAN_EVERY {
                self.rescan();
                last_scan = Instant::now();
            }
            let service_busy = lock(&self.eq).status.busy.is_some();
            if service_busy || last_apo.elapsed() >= EQ_APO_EVERY {
                self.refresh_eq_apo(false);
                self.write_stale_eq();
                last_apo = Instant::now();
            }
        }
    }

    /// Re-reads what Equalizer APO looks like on this PC (registry, service pipe, config folder).
    /// `force` = also probe the config folder's writability again (otherwise the last answer is reused).
    fn refresh_eq_apo(&self, force: bool) {
        let _one_at_a_time = lock(&self.eq_refresh);
        let outputs = lock(&self.devices).outputs.clone();
        let status = eqapo::detect(&outputs, self.flags.dry_run, force, &self.svc);
        lock(&self.eq).status = status;
    }

    /// Agent start: the first look at Equalizer APO, and the saved equalizers marked as not written yet.
    pub fn init_eq_apo(&self) {
        self.refresh_eq_apo(false); // nothing cached yet, so this probes once
        lock(&self.eq).stale = !self.flags.dry_run && !lock(&self.cfg).output_eq.is_empty();
    }

    /// Writes the saved equalizers to Equalizer APO and keeps the outcome for the dashboard and for
    /// `write_stale_eq`. The caller holds `eq_order`.
    fn write_eq(&self) -> Result<()> {
        let saved = lock(&self.cfg).output_eq.clone();
        let result = (self.eq_write)(&saved);
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        let mut eq = lock(&self.eq);
        if error.is_some() && error != eq.write_error {
            log::warn!("cannot write the output equalizer: {}", error.as_deref().unwrap_or_default());
        }
        eq.stale = error.is_some();
        eq.write_error = error;
        result
    }

    /// A write that failed (or has not happened yet) is tried again once Equalizer APO is installed and
    /// somebody can write. Never while the service is installing: its installer is still writing `config.txt`.
    fn write_stale_eq(&self) {
        let ready = {
            let eq = lock(&self.eq);
            eq.stale && eq.status.installed && eq.status.writer != "none" && eq.status.busy.is_none()
        };
        if ready {
            let _order = lock(&self.eq_order);
            let _ = self.write_eq(); // the outcome is kept in `eq` and logged there
        }
    }

    /// The three service commands (install, attach, restart) are refused under `--dry-run` and when the
    /// Clarim service is not there (a normal user cannot do any of them).
    fn service_ready(&self) -> Result<()> {
        if self.flags.dry_run {
            bail!(eqapo::MSG_DRY_RUN);
        }
        if !(self.svc.available)() {
            bail!(eqapo::MSG_NO_SERVICE);
        }
        Ok(())
    }

    fn eq_status(&self) -> EqApoStatus {
        let eq = lock(&self.eq);
        EqApoStatus { error: eq.status.error.clone().or_else(|| eq.write_error.clone()), ..eq.status.clone() }
    }

    fn tick_mic(&self, retry: &mut Retry, watch: &mut Watch) {
        let mut eng = lock(&self.mic);
        self.fold_calibration(&mut eng);
        let want = engine_settings(&lock(&self.cfg).mic, self.flags.dry_run);
        let status = eng.status();
        if !want.enabled || status.running {
            *retry = Retry::new();
        } else if retry.due() {
            retry.bump(status.error.as_deref().is_some_and(clarim_audio::is_reconnecting));
            eng.apply(&want); // same settings while dead = re-resolve the devices and try again
        }
        let captures = lock(&self.routing).as_ref().map(RoutingEngine::captures).unwrap_or_default();
        self.watch_call(&mut eng, watch, &want, &status, &captures, Instant::now());
    }

    /// The virtual mic's capture endpoint: the one calls read, the partner of the render side Clarim writes into.
    fn cable_capture_id(&self, output_name: Option<&str>) -> Option<String> {
        let devices = lock(&self.devices);
        output_name.and_then(|name| virtual_mic_capture(&devices.inputs, name)).map(|d| d.id.clone())
    }

    /// The speaker references to listen to while in a call (see [`pick_references`]); empty when nothing is left.
    fn reference_ids(&self, eng: &MicEngine) -> Vec<String> {
        let channels: Vec<Option<String>> = lock(&self.cfg).channels.values().map(|c| c.device_id.clone()).collect();
        let (console, comms) = (default_render_id(false), default_render_id(true));
        let own = [eng.output_device_id(), eng.monitor_device_id()];
        pick_references(&channels, console.as_deref(), comms.as_deref(), &lock(&self.devices).outputs, &own)
    }

    /// The call-aware part of the housekeeping tick (design 6.2), after the mic work and still under `mic`:
    /// who is in a call, the speaker references for the echo detector, the leak alerts and their balloons, the
    /// real mic's Windows volume, then what the dashboard and the tray tooltip show. `st` is the status read at
    /// the start of the tick; `captures` is the routing worker's last scan of app microphone sessions.
    fn watch_call(
        &self,
        eng: &mut MicEngine,
        watch: &mut Watch,
        want: &MicSettings,
        st: &MicStatus,
        captures: &[CaptureSession],
        now: Instant,
    ) {
        // 1-2. Call state.
        let cable = self.cable_capture_id(st.output_name.as_deref());
        let call = watch.tracker.update(now, captures, cable.as_deref());
        watch.track_call(call.in_call, &call.apps);

        // 3. References: only while in a call with the warnings on and the mic running.
        let want_refs = call.in_call && want.leak_warnings && st.running;
        let ids = if want_refs { self.reference_ids(eng) } else { Vec::new() };
        if want_refs && ids.is_empty() && !watch.refs_off_logged {
            log::info!("echo detection is off for this call: no speaker output to listen to");
            watch.refs_off_logged = true;
        }
        eng.set_references(&ids);

        // 4. Leak alerts and balloons.
        let measures = LeakMeasures {
            echo_score: st.echo_score,
            echo_db: st.echo_db,
            echo_lag_ms: st.echo_lag_ms,
            noise_db: st.noise_db,
            references: st.references,
        };
        let leak = watch.monitor.tick(now, call.in_call, want.leak_warnings, &measures);
        if let Some(kind) = leak.balloon {
            let (title, text) = leak_balloon(kind);
            if tray::notify(title, text, Balloon::Warning) {
                watch.monitor.delivered(kind, now);
            }
        }

        // 5. The real mic's Windows volume.
        let input_id = eng.input_device_id();
        let clips = clip_delta(watch.prev_clips, st.clip_events);
        watch.prev_clips = st.clip_events;
        let active = guard_active(want, call.in_call, st.running, input_id.as_deref());
        let action = watch.guard.tick(now, active, clips, st.input_voice_db, st.agc_gain_db);
        if let Some(id) = input_id.as_deref().filter(|_| action != VolAction::None) {
            self.move_mic_volume(eng, watch, now, id, action);
        }
        if call.in_call && watch.lowered_balloon_due && tray::notify(BALLOON_LOWERED.0, BALLOON_LOWERED.1, Balloon::Info) {
            watch.lowered_balloon_due = false;
            watch.lowered_balloon_told = true;
        }

        // 7. Tooltip, on transitions only; and the tuning line.
        let tip = tip_for(call.in_call, leak.alert);
        if tip != watch.tip {
            tray::set_tip(tip);
            watch.tip = tip;
        }
        if call.in_call && watch.last_call_log.is_none_or(|t| now.saturating_duration_since(t) >= CALL_LOG_EVERY) {
            watch.last_call_log = Some(now);
            log_call_line(st);
        }

        // 6. What the dashboard sees.
        *lock(&self.call) = CallOverlay {
            in_call: call.in_call,
            apps: call.apps,
            leak: leak.alert,
            note: watch.note_text(now),
            too_quiet: watch.guard.too_quiet(),
        };
    }

    /// Carries out what the guard asked for on the Windows volume of the mic `id`: read it, lower it or give some
    /// back, tell the guard what really changed, keep the record, and make the DSP follow the new input level.
    /// Any Windows error switches the guard off for this run.
    fn move_mic_volume(&self, eng: &mut MicEngine, watch: &mut Watch, now: Instant, id: &str, action: VolAction) {
        // The Raise that gives back a lowering that did not help comes with the guard already off.
        let ineffective_give_back = matches!(action, VolAction::Raise(_)) && watch.guard.disabled() == Some(GuardOff::Ineffective);
        if ineffective_give_back {
            watch.note_for_the_run(MSG_SATURATING);
        }
        let record = lock(&self.cfg).mic.mic_volume_taken.clone();
        let v = match (self.volume.read)(id) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("cannot read the mic volume in Windows, leaving it alone from now on: {e:#}");
                watch.guard.disable(GuardOff::Error);
                return;
            }
        };
        let (target, record) = match plan_volume(action, &v, record.as_ref(), id) {
            VolPlan::Skip => return,
            VolPlan::NoHardware => {
                log::info!("the mic has no usable Windows volume control: its clipping is left to the user");
                watch.guard.disable(GuardOff::NoHardware);
                watch.note_for_the_run(MSG_SATURATING);
                return;
            }
            VolPlan::Forget => {
                self.forget_taken(watch, now);
                return;
            }
            VolPlan::Set { target, record } => (target, record),
        };
        let read_back = match (self.volume.write)(id, target) {
            Ok(db) => db,
            Err(e) => {
                log::warn!("cannot set the mic volume in Windows, leaving it alone from now on: {e:#}");
                watch.guard.disable(GuardOff::Error);
                return;
            }
        };
        let delta = if (read_back - v.db).is_finite() { read_back - v.db } else { 0.0 };
        watch.guard.applied(delta, now);
        let moved = delta.abs() >= VOLUME_EPSILON_DB;
        let taken = watch.guard.taken_db();
        let record = (taken > 0.0).then_some(MicVolumeTaken { taken_db: taken, ..record });
        log::info!("mic volume in Windows: {:.1} -> {read_back:.1} dB ({action:?}), {taken:.1} dB taken in total", v.db);

        // The input level moved by `delta`: the AGC's learned level and the Voice Focus calibration follow.
        if delta != 0.0 && !eng.shift_mic_volume(delta) {
            log::debug!("the voice AGC was not told about the mic volume change (no running pipeline or queue full)");
        }
        let settings = {
            let mut cfg = lock(&self.cfg);
            cfg.mic.mic_volume_taken = record;
            if let Some(db) = cfg.mic.voice_level_db.as_mut() {
                *db += delta;
            }
            let _ = self.persist(&cfg); // logged there; the change is live either way
            cfg.mic.clone()
        };
        eng.apply(&engine_settings(&settings, self.flags.dry_run));

        if !moved {
            return;
        }
        match action {
            VolAction::Lower(db) => {
                let text = format!("Baixei o volume do microfone no Windows em {} dB porque estava estourando.", fmt_db(db));
                watch.note_for_a_minute(text, now);
                watch.lowered_balloon_due |= !watch.lowered_balloon_told;
            }
            VolAction::Raise(db) if !ineffective_give_back => {
                let text = format!("Devolvi {} dB ao volume do microfone no Windows (a voz estava baixa).", fmt_db(db));
                watch.note_for_a_minute(text, now);
            }
            _ => {}
        }
    }

    /// The volume is back where the user wants it (or the record is gone): forget what was taken, in the guard
    /// and in the config.
    fn forget_taken(&self, watch: &mut Watch, now: Instant) {
        watch.guard.applied(watch.guard.taken_db(), now);
        let mut cfg = lock(&self.cfg);
        if cfg.mic.mic_volume_taken.take().is_some() {
            let _ = self.persist(&cfg);
        }
    }

    /// Quit: gives back the Windows mic volume Clarim took, best effort. Only when the volume is still where
    /// Clarim left it (within 0.5 dB of baseline - taken): a volume the user moved meanwhile is theirs, and
    /// then only the record goes. A mic that is not plugged in keeps its record for the next run, and so does
    /// a failed write. Never under `--dry-run`.
    fn give_back_mic_volume(&self) {
        if self.flags.dry_run {
            return;
        }
        let Some(record) = lock(&self.cfg).mic.mic_volume_taken.clone() else { return };
        if !lock(&self.devices).inputs.iter().any(|d| d.id == record.device_id) {
            return;
        }
        let mut delta = 0.0;
        match (self.volume.read)(&record.device_id) {
            Ok(v) if (v.db - (record.baseline_db - record.taken_db)).abs() <= VOLUME_EPSILON_DB => {
                match (self.volume.write)(&record.device_id, record.baseline_db) {
                    Ok(db) => delta = db - v.db,
                    Err(e) => {
                        log::warn!("cannot give the mic volume back in Windows: {e:#}");
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("cannot read the mic volume in Windows to give it back: {e:#}");
                return;
            }
        }
        // The calibrated voice level follows the volume, as when it was lowered.
        let mut cfg = lock(&self.cfg);
        cfg.mic.mic_volume_taken = None;
        if let Some(db) = cfg.mic.voice_level_db.as_mut().filter(|_| delta.is_finite()) {
            *db += delta;
        }
        let _ = self.persist(&cfg);
    }

    pub fn spawn_housekeeping(self: &Arc<Self>) -> Result<()> {
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("clarim-housekeeping".into())
            .spawn(move || me.housekeep())
            .map_err(|e| anyhow!("cannot start the housekeeping thread: {e}"))?;
        *lock(&self.housekeeper) = Some(handle);
        Ok(())
    }

    /// Stops housekeeping, gives the real mic's Windows volume back, stops the mic engine and the routing worker
    /// (routes already written stay in Windows, see `clarim reset-routing`). Idempotent.
    pub fn shutdown(&self) {
        let _ = self.stop.0.try_send(());
        if let Some(h) = lock(&self.housekeeper).take() {
            let _ = h.join();
        }
        self.give_back_mic_volume(); // housekeeping is gone: nothing else moves that volume now
        lock(&self.mic).stop();
        drop(lock(&self.routing).take());
    }

    /// Tray "Pausar / Ativar microfone": flips `mic.enabled`, returns the new value.
    pub fn toggle_mic(&self) -> Result<bool> {
        if self.flags.dry_run {
            bail!(MSG_DRY_RUN_MIC);
        }
        let mut s = lock(&self.cfg).mic.clone();
        s.enabled = !s.enabled;
        let enabled = s.enabled;
        self.set_mic(s)?;
        Ok(enabled)
    }
}

impl Backend for AppBackend {
    fn snapshot(&self) -> Snapshot {
        let config = self.effective_config();
        let (inputs, all_outputs) = {
            let d = lock(&self.devices);
            (d.inputs.clone(), d.outputs.clone())
        };
        let outputs = visible(&all_outputs, &config.deny_devices);
        let virtual_mic_found = clarim_audio::find_virtual_mic(&outputs).is_some()
            || config.mic.output_id.as_ref().is_some_and(|id| all_outputs.iter().any(|d| &d.id == id));
        Snapshot {
            version: env!("CARGO_PKG_VERSION").to_string(),
            inputs: visible(&inputs, &config.deny_devices),
            outputs,
            sessions: self.sessions(),
            mic: self.mic_status(),
            virtual_mic_found,
            config,
            eq_apo: self.eq_status(),
        }
    }

    fn meters(&self) -> Meters {
        let sessions = lock(&self.routing).as_ref().map(RoutingEngine::peaks).unwrap_or_default();
        Meters { mic: self.mic_status(), sessions }
    }

    fn set_mic(&self, s: MicSettings) -> Result<()> {
        let mut eng = lock(&self.mic);
        let (applied, saved) = {
            let mut cfg = lock(&self.cfg);
            let s = check_mic(s, &cfg.mic, self.flags.dry_run)?;
            cfg.mic = s.clone();
            (s, self.persist(&cfg))
        };
        eng.apply(&engine_settings(&applied, self.flags.dry_run));
        drop(eng);
        tray::set_mic_paused(self.mic_paused());
        saved
    }

    fn set_channel(&self, id: ChannelId, mut s: ChannelSettings) -> Result<()> {
        if !s.volume.is_finite() {
            bail!("Volume inválido.");
        }
        s.volume = s.volume.clamp(0.0, 1.0);
        s.device_id = clean_id(s.device_id);
        self.edit(|cfg| {
            cfg.channels.insert(id, s);
            Ok(())
        })
    }

    fn assign_app(&self, exe: &str, ch: Option<ChannelId>) -> Result<()> {
        let exe = normalize_exe(exe)?;
        self.edit(|cfg| {
            match ch {
                Some(ch) => {
                    if !cfg.app_rules.contains_key(&exe) && cfg.app_rules.len() >= MAX_APP_RULES {
                        bail!("Limite de {MAX_APP_RULES} regras de programas atingido. Remova alguma antes de adicionar outra.");
                    }
                    cfg.app_rules.insert(exe, ch);
                }
                None => {
                    cfg.app_rules.remove(&exe);
                }
            }
            Ok(())
        })
    }

    fn set_routing_enabled(&self, on: bool) -> Result<()> {
        if self.observe_only() {
            // the dashboard shows routing as off here, so "off" changes nothing and "on" is refused
            return if on { Err(anyhow!(MSG_OBSERVE_ONLY)) } else { Ok(()) };
        }
        self.edit(|cfg| {
            cfg.routing_enabled = on;
            Ok(())
        })
    }

    fn start_calibration(&self, seconds: f32) -> Result<()> {
        if !seconds.is_finite() {
            bail!("Duração inválida.");
        }
        if self.mic_paused() {
            bail!("O microfone está desligado.");
        }
        let eng = lock(&self.mic);
        if !eng.status().running {
            bail!("Calibração indisponível: o microfone não está ativo.");
        }
        eng.start_calibration(seconds.clamp(1.0, 30.0));
        Ok(())
    }

    fn set_output_eq(&self, device_id: &str, eq: Option<OutputEq>) -> Result<()> {
        if self.flags.dry_run {
            bail!(eqapo::MSG_DRY_RUN); // the dashboard shows the equalizer as unavailable here
        }
        let eq = eq.map(OutputEq::clamped).transpose()?;
        let _order = lock(&self.eq_order);
        let saved = {
            let mut cfg = lock(&self.cfg);
            match eq {
                Some(eq) => {
                    if endpoint_guid(device_id).is_none() {
                        bail!("Dispositivo de saída inválido.");
                    }
                    if !cfg.output_eq.contains_key(device_id) && cfg.output_eq.len() >= MAX_OUTPUT_EQ {
                        bail!("Limite de {MAX_OUTPUT_EQ} dispositivos com equalizador atingido. Remova algum antes de adicionar outro.");
                    }
                    cfg.output_eq.insert(device_id.to_string(), eq);
                }
                None => {
                    cfg.output_eq.remove(device_id); // always allowed: also clears a stale hand-edited key
                }
            }
            self.persist(&cfg)
        };
        // the change is in the config either way; a failed write is retried by housekeeping
        saved.and(self.write_eq())
    }

    fn install_eq_apo(&self) -> Result<()> {
        self.service_ready()?;
        let cfg = lock(&self.cfg).clone();
        let outputs = lock(&self.devices).outputs.clone();
        let targets = eqapo::install_targets(&cfg, &outputs);
        (self.svc.install)(&targets)?;
        // once APO is there, the first write of clarim-eq.txt (and the Include line) happens by itself
        lock(&self.eq).stale = true;
        self.refresh_eq_apo(false); // so the dashboard sees the service's "busy" text at once
        Ok(())
    }

    fn set_eq_device(&self, device_id: &str, enabled: bool) -> Result<()> {
        self.service_ready()?;
        let Some(guid) = endpoint_guid(device_id) else { bail!("Dispositivo de saída inválido.") };
        if enabled {
            // attaching is for physical, listed devices; detaching is always allowed (the service checks the GUID)
            let deny = lock(&self.cfg).deny_devices.clone();
            let dev = lock(&self.devices).outputs.iter().find(|d| d.id == device_id).cloned();
            match dev {
                None => bail!("Aparelho de saída não encontrado. Atualize a lista de dispositivos."),
                Some(d) if !eqapo::eq_capable(&d, &deny) => bail!("Este aparelho não aceita o equalizador (é um dispositivo virtual)."),
                Some(_) => {}
            }
        }
        let result = (self.svc.set_device)(guid, enabled);
        self.refresh_eq_apo(false); // `needs_restart` shows at once
        result
    }

    fn restart_audio(&self) -> Result<()> {
        self.service_ready()?;
        (self.svc.restart)()?;
        self.refresh_eq_apo(false);
        Ok(())
    }

    fn refresh_devices(&self) {
        self.rescan();
        self.refresh_eq_apo(true);
        // also makes the routing worker re-enumerate the app sessions right away
        self.push_routing(&lock(&self.cfg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarim_core::is_virtual_device;
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn no_save(_: &Config) -> Result<()> {
        Ok(())
    }

    fn failing_save(_: &Config) -> Result<()> {
        Err(anyhow!("disk full"))
    }

    fn no_eq_write(_: &BTreeMap<String, OutputEq>) -> Result<()> {
        Ok(())
    }

    const EQ_DEV: &str = "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}";

    fn dev(name: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo { id: format!("id-{name}"), name: name.into(), is_default, is_virtual: is_virtual_device(name) }
    }

    /// A backend that cannot touch the machine: the mic is disabled in the config (engine idle, no
    /// device opened) and routing is observe-only (`no_routing`), so nothing is ever written to Windows.
    fn safe_backend(save: SaveFn) -> AppBackend {
        let cfg = Config { mic: MicSettings { enabled: false, ..MicSettings::default() }, ..Config::default() };
        let scan = Scan {
            inputs: vec![dev("Microfone (USB)", true)],
            outputs: vec![dev("Alto-falantes (Realtek)", true), dev("CABLE Input (VB-Audio Virtual Cable)", false)],
        };
        AppBackend::new(cfg, AgentFlags { dry_run: false, no_routing: true }, scan, save)
            .with_eq_write(no_eq_write)
            .with_service(calm_service())
    }

    /// A Clarim service that is there, has nothing to report and accepts every command: tests never touch the pipe.
    fn calm_service() -> Service {
        Service {
            available: || true,
            status: || Ok(clarim_svc::ApoServiceStatus::default()),
            install: |_| Ok(()),
            set_device: |_, _| Ok(()),
            restart: || Ok(()),
        }
    }

    #[test]
    fn mic_settings_are_clamped_and_nan_is_refused() {
        let stored = MicSettings { voice_level_db: Some(-23.0), ..MicSettings::default() };
        let wild = MicSettings {
            denoise_strength: 7.0,
            voice_focus_db: -99.0,
            gate_threshold_db: 5.0,
            input_gain_db: 50.0,
            output_gain_db: -50.0,
            voice_level_db: Some(-1.0),
            eq_bass_db: 30.0,
            eq_mid_db: -30.0,
            eq_treble_db: 2.5,
            input_id: Some("  ".into()),
            output_id: Some("cable".into()),
            ..MicSettings::default()
        };
        let ok = check_mic(wild, &stored, false).expect("clamped, not refused");
        assert_eq!((ok.denoise_strength, ok.voice_focus_db, ok.gate_threshold_db), (1.0, -40.0, 0.0));
        assert_eq!((ok.input_gain_db, ok.output_gain_db), (20.0, -20.0));
        assert_eq!((ok.eq_bass_db, ok.eq_mid_db, ok.eq_treble_db), (12.0, -12.0, 2.5), "tone sliders clamp to +-12");
        assert_eq!(ok.voice_level_db, Some(-23.0), "voice level comes from the calibration, not the request");
        assert_eq!((ok.input_id, ok.output_id.as_deref()), (None, Some("cable")));

        let nan = MicSettings { input_gain_db: f32::NAN, ..MicSettings::default() };
        assert!(check_mic(nan, &stored, false).is_err());
        let inf = MicSettings { denoise_strength: f32::INFINITY, ..MicSettings::default() };
        assert!(check_mic(inf, &stored, false).is_err());
        let nan_tone = MicSettings { eq_mid_db: f32::NAN, ..MicSettings::default() };
        assert!(check_mic(nan_tone, &stored, false).is_err());
    }

    #[test]
    fn dry_run_refuses_enabling_the_mic_and_keeps_the_saved_flag() {
        let stored = MicSettings { enabled: true, ..MicSettings::default() };
        let seen_by_dashboard = MicSettings { enabled: false, denoise: clarim_core::DenoiseMode::Light, ..stored.clone() };
        let ok = check_mic(seen_by_dashboard, &stored, true).expect("other edits are fine");
        assert!(ok.enabled, "the saved `enabled` is untouched by the override");
        assert_eq!(ok.denoise, clarim_core::DenoiseMode::Light);
        assert!(check_mic(MicSettings { enabled: true, ..stored.clone() }, &stored, true).is_err());
        assert!(!engine_settings(&stored, true).enabled, "the engine never sees enabled under --dry-run");
        assert!(engine_settings(&stored, false).enabled);
    }

    #[test]
    fn exe_names_are_normalised_or_refused() {
        assert_eq!(normalize_exe("  Spotify.EXE ").ok().as_deref(), Some("spotify.exe"));
        for bad in [
            "",
            "   ",
            r"C:\Games\game.exe",
            "a/b.exe",
            &"x".repeat(261),
            "a\0b.exe",
            "a\tb.exe",
            "a\u{7f}b.exe",
            "c:game.exe",
            "<a>.exe",
            "a\"b.exe",
            "a|b.exe",
            "a?.exe",
            "a*.exe",
        ] {
            assert!(normalize_exe(bad).is_err(), "{bad:?}");
        }
        // spaces, dots, dashes and non-ASCII letters are fine in a file name
        assert_eq!(normalize_exe("Meu Jogo-2.EXE").ok().as_deref(), Some("meu jogo-2.exe"));
    }

    #[test]
    fn a_new_rule_is_refused_at_the_cap_but_existing_ones_stay_editable() {
        let b = safe_backend(no_save);
        {
            let mut cfg = lock(&b.cfg);
            cfg.app_rules = (0..MAX_APP_RULES).map(|i| (format!("app{i}.exe"), ChannelId::Media)).collect();
        }
        let err = b.assign_app("one-more.exe", Some(ChannelId::Chat)).expect_err("the 513th key is refused");
        assert!(err.to_string().contains("512"), "{err}");
        assert!(!lock(&b.cfg).app_rules.contains_key("one-more.exe"));
        b.assign_app("app7.exe", Some(ChannelId::Chat)).expect("an existing key can change channel");
        assert_eq!(lock(&b.cfg).app_rules.get("app7.exe"), Some(&ChannelId::Chat));
        b.assign_app("app8.exe", None).expect("removing is always allowed");
        b.assign_app("one-more.exe", Some(ChannelId::Chat)).expect("room again after a removal");
        assert_eq!(lock(&b.cfg).app_rules.len(), MAX_APP_RULES);
    }

    #[test]
    fn routing_is_only_enabled_when_asked_and_not_overridden() {
        let mut cfg = Config::default();
        assert!(routing_config(&cfg, false).enabled);
        assert!(!routing_config(&cfg, true).enabled);
        cfg.routing_enabled = false;
        assert!(!routing_config(&cfg, false).enabled);
        assert_eq!(routing_config(&cfg, false).app_rules, cfg.app_rules);
    }

    #[test]
    fn first_run_defaults_avoid_a_virtual_default_device() {
        let outputs = [
            dev("Áudio Digital (S/PDIF) (Realtek(R) Audio)", false),
            dev("SteelSeries Sonar - Gaming (SteelSeries Sonar Virtual Audio Device)", true),
            dev("Speakers (NVIDIA Broadcast)", false),
            dev("Alto-falantes (Realtek)", false),
        ];
        let mut cfg = Config::default();
        assert!(cfg.routing_enabled);
        first_run_defaults(&mut cfg, &outputs);
        for c in ChannelId::ALL {
            assert_eq!(cfg.channels[&c].device_id.as_deref(), Some("id-Alto-falantes (Realtek)"), "{c:?}");
        }
        assert!(!cfg.routing_enabled, "Sonar is installed: the first launch must not touch app routing");

        // only a digital output is left: better than nothing
        let outputs = [dev("Voicemeeter Input", true), dev("Áudio Digital (S/PDIF) (Realtek(R) Audio)", false)];
        let mut cfg = Config::default();
        first_run_defaults(&mut cfg, &outputs);
        assert!(cfg.channels.values().all(|c| c.device_id.as_deref() == Some("id-Áudio Digital (S/PDIF) (Realtek(R) Audio)")));

        assert!(cfg.routing_enabled, "no Sonar here: routing keeps its default");

        // real default device: channels stay on "Windows default"
        let outputs = [dev("Alto-falantes (Realtek)", true), dev("CABLE Input (VB-Audio Virtual Cable)", false)];
        let mut cfg = Config::default();
        first_run_defaults(&mut cfg, &outputs);
        assert!(cfg.channels.values().all(|c| c.device_id.is_none()));
        assert!(cfg.routing_enabled);

        // Sonar present but not the default device: channels untouched, routing still starts off
        let outputs = [dev("Alto-falantes (Realtek)", true), dev("SteelSeries SONAR - Microphone (Sonar)", false)];
        let mut cfg = Config::default();
        first_run_defaults(&mut cfg, &outputs);
        assert!(cfg.channels.values().all(|c| c.device_id.is_none()));
        assert!(!cfg.routing_enabled, "the check is case-insensitive and does not need the default to be Sonar");

        // virtual default and nothing real to pick: also left alone
        let mut cfg = Config::default();
        first_run_defaults(&mut cfg, &[dev("Voicemeeter Input", true)]);
        assert!(cfg.channels.values().all(|c| c.device_id.is_none()));
    }

    #[test]
    fn setters_validate_apply_and_report_save_failures() {
        let b = safe_backend(no_save);
        b.set_channel(ChannelId::Chat, ChannelSettings { device_id: Some(String::new()), volume: 3.0, muted: true })
            .expect("set_channel");
        b.assign_app("  Game.EXE", Some(ChannelId::Aux)).expect("assign");
        let snap = b.snapshot();
        assert_eq!(snap.config.channels[&ChannelId::Chat], ChannelSettings { device_id: None, volume: 1.0, muted: true });
        assert_eq!(snap.config.app_rules.get("game.exe"), Some(&ChannelId::Aux));
        assert!(b.set_channel(ChannelId::Chat, ChannelSettings { volume: f32::NAN, ..Default::default() }).is_err());
        assert!(b.assign_app(r"c:\x\y.exe", Some(ChannelId::Aux)).is_err());
        b.assign_app("game.exe", None).expect("remove rule");
        assert!(!b.snapshot().config.app_rules.contains_key("game.exe"));

        // a failing save is reported, but the change is live
        let b = safe_backend(failing_save);
        let err = b.set_channel(ChannelId::Media, ChannelSettings { volume: 0.25, ..Default::default() });
        assert!(err.is_err_and(|e| e.to_string().contains("salvar")));
        assert_eq!(b.snapshot().config.channels[&ChannelId::Media].volume, 0.25);
    }

    #[test]
    fn output_eq_is_validated_stored_and_removable() {
        const DEV: &str = EQ_DEV;
        let b = safe_backend(no_save);
        b.set_output_eq(DEV, Some(OutputEq { bass_db: 50.0, mid_db: 0.0, treble_db: -2.0 })).expect("set");
        assert_eq!(b.snapshot().config.output_eq[DEV], OutputEq { bass_db: 12.0, mid_db: 0.0, treble_db: -2.0 });
        // neutral is kept as an entry (the dashboard shows "Neutro"), not dropped
        b.set_output_eq(DEV, Some(OutputEq::default())).expect("neutral");
        assert_eq!(b.snapshot().config.output_eq[DEV], OutputEq::default());
        // bad ids and NaN are refused and change nothing
        let injected = format!("{DEV}\nInclude: x");
        for bad in ["", "out-rtk", "{0.0.0.00000000}.abc", injected.as_str()] {
            assert!(b.set_output_eq(bad, Some(OutputEq::default())).is_err(), "{bad:?}");
        }
        assert!(b.set_output_eq(DEV, Some(OutputEq { bass_db: f32::NAN, ..OutputEq::default() })).is_err());
        assert_eq!(b.snapshot().config.output_eq.len(), 1);
        // removal, also of a key that is not a valid endpoint id
        lock(&b.cfg).output_eq.insert("stale".into(), OutputEq::default());
        b.set_output_eq("stale", None).expect("remove stale");
        b.set_output_eq(DEV, None).expect("remove");
        assert!(b.snapshot().config.output_eq.is_empty());
        // cap: a new key is refused at the limit, existing keys stay editable
        let guid = |i: usize| format!("{{0.0.0.00000000}}.{{{i:08x}-0000-0000-0000-000000000000}}");
        for i in 0..MAX_OUTPUT_EQ {
            b.set_output_eq(&guid(i), Some(OutputEq::default())).expect("under the cap");
        }
        assert!(b.set_output_eq(&guid(MAX_OUTPUT_EQ), Some(OutputEq::default())).is_err());
        b.set_output_eq(&guid(3), Some(OutputEq { mid_db: 1.0, ..OutputEq::default() })).expect("existing key");
        // a failing save is reported, the change is live
        let b = safe_backend(failing_save);
        assert!(b.set_output_eq(DEV, Some(OutputEq::default())).is_err_and(|e| e.to_string().contains("salvar")));
        assert!(b.snapshot().config.output_eq.contains_key(DEV));
    }

    #[test]
    fn a_failed_equalizer_write_is_reported_kept_and_retried_once_somebody_can_write() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn first_write_fails(_: &BTreeMap<String, OutputEq>) -> Result<()> {
            if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(anyhow!("sem permissão"))
            } else {
                Ok(())
            }
        }
        let b = safe_backend(no_save).with_eq_write(first_write_fails);
        let eq = OutputEq { bass_db: 5.0, ..OutputEq::default() };
        let err = b.set_output_eq(EQ_DEV, Some(eq)).expect_err("the first write fails");
        assert!(err.to_string().contains("sem permissão"), "{err}");
        assert_eq!(b.snapshot().config.output_eq[EQ_DEV], eq, "the change stays in the config");
        assert!(b.snapshot().eq_apo.error.as_deref().is_some_and(|e| e.contains("sem permissão")));
        // nobody is known to be able to write: housekeeping leaves it alone
        b.write_stale_eq();
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        // Equalizer APO installed and the service answers: the retry writes and the error is gone
        lock(&b.eq).status = EqApoStatus { installed: true, writer: "service".into(), ..EqApoStatus::default() };
        b.write_stale_eq();
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(b.snapshot().eq_apo.error, None);
        b.write_stale_eq();
        assert_eq!(CALLS.load(Ordering::SeqCst), 2, "nothing stale: no more writes");
    }

    // ---- the three service commands (install, attach, restart)

    const RTK: &str = "{171dffe0-1f81-4558-a253-45945204afa2}";
    const BT: &str = "{b9cc1e4e-11bd-4f54-8227-5e28f30a485d}";
    const CABLE: &str = "{aee288e6-0000-4000-8000-000000000001}";

    fn gdev(guid: &str, name: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo { id: format!("{{0.0.0.00000000}}.{guid}"), name: name.into(), is_default, is_virtual: is_virtual_device(name) }
    }

    /// Like `safe_backend`, with endpoint ids that carry a GUID (what the service commands need).
    fn pc_backend(svc: Service) -> AppBackend {
        let cfg = Config { mic: MicSettings { enabled: false, ..MicSettings::default() }, ..Config::default() };
        let scan = Scan {
            inputs: vec![],
            outputs: vec![
                gdev(RTK, "Alto-falantes (Realtek(R) Audio)", true),
                gdev(BT, "Fones de ouvido (WZ-BT5.0)", false),
                gdev(CABLE, "CABLE Input (VB-Audio Virtual Cable)", false),
            ],
        };
        AppBackend::new(cfg, AgentFlags { dry_run: false, no_routing: true }, scan, no_save)
            .with_eq_write(no_eq_write)
            .with_service(svc)
    }

    #[test]
    fn install_sends_the_channels_physical_devices_and_the_dashboard_sees_the_service_busy_at_once() {
        static SENT: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());
        let b = pc_backend(Service {
            install: |guids| {
                SENT.lock().unwrap().push(guids.to_vec());
                Ok(())
            },
            status: || {
                Ok(clarim_svc::ApoServiceStatus { busy: Some("Baixando o Equalizer APO...".into()), ..Default::default() })
            },
            ..calm_service()
        });
        // Chat on the headset, the cable on Aux (never sent), the rest on the Windows default
        let mut chat = b.snapshot().config.channels[&ChannelId::Chat].clone();
        chat.device_id = Some(format!("{{0.0.0.00000000}}.{BT}"));
        b.set_channel(ChannelId::Chat, chat).expect("set chat device");
        let mut aux = b.snapshot().config.channels[&ChannelId::Aux].clone();
        aux.device_id = Some(format!("{{0.0.0.00000000}}.{CABLE}"));
        b.set_channel(ChannelId::Aux, aux).expect("set aux device");

        b.install_eq_apo().expect("accepted");
        assert_eq!(*SENT.lock().unwrap(), [vec![RTK.to_string(), BT.to_string()]]);
        assert_eq!(b.snapshot().eq_apo.busy.as_deref(), Some("Baixando o Equalizer APO..."), "no waiting for the next 10 s look");
        assert!(lock(&b.eq).stale, "the equalizer file is written once APO is there");
    }

    #[test]
    fn the_service_commands_are_refused_without_the_service_and_under_dry_run_and_pass_the_services_own_errors() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn counted() -> Result<()> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let counting = Service {
            install: |_| counted(),
            set_device: |_, _| counted(),
            restart: counted,
            ..calm_service()
        };
        // no service
        let b = pc_backend(Service { available: || false, ..counting });
        let dev = format!("{{0.0.0.00000000}}.{RTK}");
        for err in [b.install_eq_apo(), b.set_eq_device(&dev, true), b.restart_audio()] {
            assert!(err.expect_err("refused").to_string().contains("clarim install"));
        }
        // --dry-run: nothing is sent either, whatever the service says
        let cfg = Config::default();
        let dry = AppBackend::new(cfg, AgentFlags { dry_run: true, no_routing: false }, Scan::default(), no_save)
            .with_eq_write(no_eq_write)
            .with_service(counting);
        for err in [dry.install_eq_apo(), dry.set_eq_device(&dev, false), dry.restart_audio()] {
            assert!(err.expect_err("refused").to_string().contains("--dry-run"));
        }
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);
        // the service's own refusal reaches the caller as it is (PT-BR, from the service)
        let busy = pc_backend(Service {
            install: |_| Err(anyhow!("Já há uma operação em andamento: Baixando o Equalizer APO...")),
            restart: || Err(anyhow!("O áudio do Windows acabou de ser reiniciado. Espere alguns segundos.")),
            ..counting
        });
        assert!(busy.install_eq_apo().expect_err("busy").to_string().contains("Já há uma operação"));
        assert!(busy.restart_audio().expect_err("too soon").to_string().contains("Espere"));
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn attaching_checks_the_device_here_and_sends_the_guid_not_the_endpoint_id() {
        static SENT: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
        let b = pc_backend(Service {
            set_device: |guid, on| {
                SENT.lock().unwrap().push((guid.to_string(), on));
                Ok(())
            },
            status: || Ok(clarim_svc::ApoServiceStatus { installed: true, needs_restart: true, ..Default::default() }),
            ..calm_service()
        });
        let id = |g: &str| format!("{{0.0.0.00000000}}.{g}");
        b.set_eq_device(&id(RTK), true).expect("attach");
        b.set_eq_device(&id(BT), false).expect("detach");
        assert_eq!(*SENT.lock().unwrap(), [(RTK.to_string(), true), (BT.to_string(), false)]);
        assert!(b.snapshot().eq_apo.needs_restart, "the restart notice does not wait for the next look");
        // refused here, never sent: a virtual device, a device that is not listed, ids that are not endpoint ids
        assert!(b.set_eq_device(&id(CABLE), true).expect_err("virtual").to_string().contains("virtual"));
        assert!(b.set_eq_device(&id("{deadbeef-0000-4000-8000-000000000003}"), true).is_err());
        for bad in ["", "out-rtk", "{0.0.0.00000000}.abc", &format!("{}\nx", id(RTK))] {
            assert!(b.set_eq_device(bad, true).is_err() && b.set_eq_device(bad, false).is_err(), "{bad:?}");
        }
        // detaching a device that is gone from the list is still allowed (the service checks the GUID)
        b.set_eq_device(&id("{deadbeef-0000-4000-8000-000000000003}"), false).expect("detach an unplugged device");
        assert_eq!(SENT.lock().unwrap().len(), 3);
    }

    #[test]
    fn nothing_is_written_to_equalizer_apo_while_the_service_is_installing_it() {
        static WRITES: AtomicUsize = AtomicUsize::new(0);
        fn counted(_: &BTreeMap<String, OutputEq>) -> Result<()> {
            WRITES.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let b = pc_backend(calm_service()).with_eq_write(counted);
        lock(&b.eq).stale = true;
        let status = |busy: Option<&str>| EqApoStatus {
            installed: true,
            writer: "service".into(),
            busy: busy.map(Into::into),
            ..EqApoStatus::default()
        };
        lock(&b.eq).status = status(Some("Instalando o Equalizer APO..."));
        b.write_stale_eq();
        assert_eq!(WRITES.load(Ordering::SeqCst), 0, "the installer is still writing config.txt");
        lock(&b.eq).status = status(None);
        b.write_stale_eq();
        assert_eq!(WRITES.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_output_equalizer_is_never_written_under_dry_run() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn counted(_: &BTreeMap<String, OutputEq>) -> Result<()> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let mut cfg = Config::default();
        cfg.output_eq.insert(EQ_DEV.into(), OutputEq { bass_db: 5.0, ..OutputEq::default() });
        let flags = AgentFlags { dry_run: true, no_routing: false };
        let b = AppBackend::new(cfg, flags, Scan::default(), no_save).with_eq_write(counted);
        let err = b.set_output_eq(EQ_DEV, Some(OutputEq::default())).expect_err("refused under --dry-run");
        assert!(err.to_string().contains("--dry-run"), "{err}");
        assert_eq!(b.snapshot().config.output_eq[EQ_DEV].bass_db, 5.0, "nothing changed");
        b.init_eq_apo(); // read-only look; the saved equalizer is not marked for writing
        lock(&b.eq).status = EqApoStatus { installed: true, writer: "service".into(), ..EqApoStatus::default() };
        b.write_stale_eq();
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(b.snapshot().eq_apo.writer, "service", "the cached status is what the dashboard gets");
    }

    #[test]
    fn snapshot_reports_overrides_devices_and_keeps_the_saved_values() {
        let b = safe_backend(no_save);
        let snap = b.snapshot();
        assert!(snap.virtual_mic_found);
        assert_eq!((snap.inputs.len(), snap.outputs.len()), (1, 2));
        assert!(snap.sessions.iter().all(|s| s.exe != UNKNOWN_EXE));
        assert!(!snap.mic.running && snap.mic.error.is_none(), "disabled mic is idle, not an error");
        // --no-routing: the dashboard sees routing off, enabling it is refused, "off" is a no-op
        assert!(!snap.config.routing_enabled);
        assert!(b.set_routing_enabled(true).is_err());
        assert!(b.set_routing_enabled(false).is_ok());
        assert!(lock(&b.cfg).routing_enabled, "the saved value is not overwritten by the override");
        // calibration needs a running mic
        assert!(b.start_calibration(5.0).is_err());
        assert!(b.start_calibration(f32::NAN).is_err());
    }

    #[test]
    fn virtual_mic_missing_is_reported_and_deny_list_applies() {
        let cfg = Config { mic: MicSettings { enabled: false, ..MicSettings::default() }, ..Config::default() };
        let scan = Scan {
            inputs: vec![],
            outputs: vec![dev("Alto-falantes (Realtek)", true), dev("Speakers (NVIDIA Broadcast)", false)],
        };
        let b = AppBackend::new(cfg, AgentFlags { dry_run: false, no_routing: true }, scan, no_save);
        let snap = b.snapshot();
        assert!(!snap.virtual_mic_found);
        assert_eq!(snap.outputs.len(), 1, "NVIDIA Broadcast is on the default deny-list");
    }

    #[test]
    fn dry_run_shows_mic_off_and_refuses_to_toggle_it() {
        let cfg = Config::default(); // mic enabled in the config
        let b = AppBackend::new(cfg, AgentFlags { dry_run: true, no_routing: false }, Scan::default(), no_save);
        let snap = b.snapshot();
        assert!(!snap.config.mic.enabled && !snap.config.routing_enabled);
        assert!(!snap.mic.running);
        assert!(b.mic_paused());
        assert!(b.toggle_mic().is_err());
        assert!(b.set_mic(MicSettings { enabled: true, ..MicSettings::default() }).is_err());
        b.set_mic(MicSettings { enabled: false, denoise: clarim_core::DenoiseMode::Off, ..MicSettings::default() })
            .expect("other mic edits still work");
        assert!(lock(&b.cfg).mic.enabled, "saved `enabled` survives");
        assert_eq!(lock(&b.cfg).mic.denoise, clarim_core::DenoiseMode::Off);
    }

    #[test]
    fn retry_backs_off_up_to_the_cap() {
        let mut r = Retry::new();
        assert!(!r.due());
        let mut delays = Vec::new();
        for _ in 0..6 {
            delays.push(r.delay);
            r.bump(false);
        }
        assert_eq!(delays.iter().map(Duration::as_secs).collect::<Vec<_>>(), [3, 6, 12, 24, 30, 30]);
    }

    #[test]
    fn retry_keeps_knocking_every_3_s_while_windows_audio_restarts() {
        let mut r = Retry::new();
        for _ in 0..4 {
            r.bump(false); // a mic that has been dead for a while: delay 3, 6, 12, 24 -> 30 next
        }
        assert_eq!(r.delay.as_secs(), 30);
        r.bump(true);
        assert_eq!(r.delay.as_secs(), 3, "Windows Audio is coming back: no long wait");
        r.bump(true);
        assert_eq!(r.delay.as_secs(), 3);
        r.bump(false);
        assert_eq!(r.delay.as_secs(), 6, "a real failure backs off again");
        assert!(clarim_audio::is_reconnecting("Reconectando ao áudio do Windows (o serviço de áudio parou ou foi reiniciado)"));
    }

    // ---- calls, leak alerts and the real mic's Windows volume (design 6.2)

    thread_local! {
        /// The fake hardware volume of the mic, and every value written to it. Thread-local: the tests below
        /// call the backend on their own thread, and tests run in parallel.
        static FAKE_VOL: Cell<EndpointVolume> =
            const { Cell::new(EndpointVolume { db: 0.0, min_db: 0.0, max_db: 0.0, hardware: false }) };
        static FAKE_WRITES: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    }

    fn fake_read(_: &str) -> Result<EndpointVolume> {
        Ok(FAKE_VOL.get())
    }

    fn fake_write(_: &str, db: f32) -> Result<f32> {
        let mut v = FAKE_VOL.get();
        v.db = db.clamp(v.min_db, v.max_db);
        FAKE_VOL.set(v);
        FAKE_WRITES.with_borrow_mut(|w| w.push(db));
        Ok(v.db)
    }

    fn failing_read(_: &str) -> Result<EndpointVolume> {
        Err(anyhow!("endpoint gone"))
    }

    fn failing_write(_: &str, _: f32) -> Result<f32> {
        Err(anyhow!("access denied"))
    }

    const MIC_ID: &str = "id-Microfone (USB)";
    const CABLE_RENDER: &str = "CABLE Input (VB-Audio Virtual Cable)";
    const CABLE_CAPTURE_ID: &str = "id-CABLE Output (VB-Audio Virtual Cable)";

    /// A mic with a real hardware volume of -30..0 dB, now at `db`.
    fn hardware(db: f32) -> EndpointVolume {
        EndpointVolume { db, min_db: -30.0, max_db: 0.0, hardware: true }
    }

    /// `safe_backend` (idle mic engine) whose mic has the fake Windows volume `vol`.
    fn volume_backend(vol: EndpointVolume) -> AppBackend {
        FAKE_VOL.set(vol);
        FAKE_WRITES.with_borrow_mut(Vec::clear);
        safe_backend(no_save).with_volume(MicVolume { read: fake_read, write: fake_write })
    }

    fn writes() -> Vec<f32> {
        FAKE_WRITES.with_borrow(Clone::clone)
    }

    fn taken_record(device_id: &str, baseline_db: f32, taken_db: f32) -> MicVolumeTaken {
        MicVolumeTaken { device_id: device_id.into(), baseline_db, taken_db }
    }

    fn set_record(b: &AppBackend, baseline_db: f32, taken_db: f32) {
        lock(&b.cfg).mic.mic_volume_taken = Some(taken_record(MIC_ID, baseline_db, taken_db));
    }

    fn record(b: &AppBackend) -> Option<MicVolumeTaken> {
        lock(&b.cfg).mic.mic_volume_taken.clone()
    }

    /// `move_mic_volume` on the idle test engine, for the mic `MIC_ID`.
    fn move_volume(b: &AppBackend, w: &mut Watch, now: Instant, action: VolAction) {
        let mut eng = lock(&b.mic);
        b.move_mic_volume(&mut eng, w, now, MIC_ID, action);
    }

    fn secs(t: Instant, s: u64) -> Instant {
        t + Duration::from_secs(s)
    }

    #[test]
    fn lowering_the_mic_volume_records_the_baseline_and_the_dsp_and_voice_level_follow() {
        let b = volume_backend(hardware(-10.0));
        lock(&b.cfg).mic.voice_level_db = Some(-20.0);
        let (mut w, now) = (Watch::new(0.0), Instant::now());
        let action = w.guard.tick(now, true, 5, None, 0.0);
        assert_eq!(action, VolAction::Lower(3.0), "five clipped frames in a tick ask for a lowering");
        move_volume(&b, &mut w, now, action);
        assert_eq!(writes(), [-13.0]);
        assert_eq!(record(&b), Some(taken_record(MIC_ID, -10.0, 3.0)), "the baseline is the volume before the first lowering");
        assert_eq!(w.guard.taken_db(), 3.0);
        assert_eq!(lock(&b.cfg).mic.voice_level_db, Some(-23.0), "the calibrated voice level follows the input level");
        let note = w.note_text(now).unwrap_or_default();
        assert!(note.starts_with("Baixei o volume do microfone no Windows em 3 dB"), "{note}");
        assert!(w.lowered_balloon_due, "the one balloon of this call is wanted");
        assert_eq!(b.snapshot().config.mic.mic_volume_taken, record(&b), "the dashboard sees the record (and cannot edit it)");

        // a second lowering keeps the first baseline
        FAKE_VOL.set(hardware(-13.0));
        move_volume(&b, &mut w, secs(now, 10), VolAction::Lower(3.0));
        assert_eq!(record(&b), Some(taken_record(MIC_ID, -10.0, 6.0)));
    }

    #[test]
    fn a_mic_without_a_hardware_volume_is_left_alone_for_the_run_with_a_note_that_stays() {
        for vol in [EndpointVolume { hardware: false, ..hardware(-10.0) }, EndpointVolume { min_db: -2.0, ..hardware(-1.0) }] {
            let b = volume_backend(vol);
            let (mut w, now) = (Watch::new(0.0), Instant::now());
            move_volume(&b, &mut w, now, VolAction::Lower(3.0));
            assert_eq!(w.guard.disabled(), Some(GuardOff::NoHardware), "{vol:?}");
            assert!(writes().is_empty() && record(&b).is_none(), "{vol:?}: nothing is touched");
            assert!(!w.lowered_balloon_due, "{vol:?}: nothing was lowered, nothing to announce");
            assert_eq!(w.note_text(secs(now, 3600)).as_deref(), Some(MSG_SATURATING), "{vol:?}: the note stays for the run");
        }
    }

    #[test]
    fn a_mic_already_at_the_bottom_of_its_range_is_left_alone_for_the_run_with_a_note_that_stays() {
        for db in [-30.0, -29.7] {
            let b = volume_backend(hardware(db)); // nothing left to lower: under half a dB above the floor
            let (mut w, now) = (Watch::new(0.0), Instant::now());
            move_volume(&b, &mut w, now, VolAction::Lower(3.0));
            assert!(writes().is_empty(), "{db} dB: no write, so no config rewrite every 10 s");
            assert_eq!(record(&b), None, "{db} dB: a record of 0 dB taken is no record");
            assert_eq!(w.guard.disabled(), Some(GuardOff::NoHardware), "{db} dB");
            assert!(!w.lowered_balloon_due, "{db} dB: nothing was lowered, nothing to announce");
            assert_eq!(w.note_text(secs(now, 3600)).as_deref(), Some(MSG_SATURATING), "{db} dB: the user is told, for the run");
            assert_eq!(w.guard.tick(secs(now, 20), true, 9, None, 0.0), VolAction::None, "{db} dB: the guard asks no more");
        }
    }

    #[test]
    fn giving_back_goes_in_steps_never_above_the_baseline_and_drops_the_record_at_zero() {
        let b = volume_backend(hardware(-16.0));
        set_record(&b, -10.0, 6.0);
        lock(&b.cfg).mic.voice_level_db = Some(-30.0);
        let (mut w, now) = (Watch::new(6.0), Instant::now());
        move_volume(&b, &mut w, now, VolAction::Raise(3.0));
        assert_eq!(writes(), [-13.0]);
        assert_eq!(record(&b), Some(taken_record(MIC_ID, -10.0, 3.0)));
        assert_eq!(lock(&b.cfg).mic.voice_level_db, Some(-27.0), "the voice level follows the volume back up");
        let note = w.note_text(now).unwrap_or_default();
        assert!(note.starts_with("Devolvi 3 dB ao volume do microfone no Windows"), "{note}");

        move_volume(&b, &mut w, secs(now, 10), VolAction::Raise(3.0));
        assert_eq!(writes(), [-13.0, -10.0]);
        assert_eq!(record(&b), None, "all given back: the record is gone");
        assert_eq!(w.guard.taken_db(), 0.0);
    }

    #[test]
    fn a_volume_the_user_put_back_ends_the_giving_back_and_forgets_what_was_taken() {
        let b = volume_backend(hardware(-10.2)); // within 0.5 dB of the baseline
        set_record(&b, -10.0, 3.0);
        let (mut w, now) = (Watch::new(3.0), Instant::now());
        move_volume(&b, &mut w, now, VolAction::Raise(3.0));
        assert!(writes().is_empty(), "the volume is not ours to touch");
        assert_eq!(record(&b), None);
        assert_eq!(w.guard.taken_db(), 0.0, "the guard stops asking to give back");
        assert!(w.note_text(now).is_none());
    }

    #[test]
    fn an_ineffective_lowering_is_given_back_through_the_real_guard_with_the_note_for_the_run() {
        let b = volume_backend(hardware(-10.0));
        let (mut w, t0) = (Watch::new(0.0), Instant::now());
        let lower = w.guard.tick(t0, true, 5, None, 0.0);
        move_volume(&b, &mut w, t0, lower);
        assert_eq!(record(&b).map(|r| r.taken_db), Some(3.0));
        // ten more ticks at the same clipping: the volume was not the cause
        let mut action = VolAction::None;
        for s in 1..=10 {
            action = w.guard.tick(secs(t0, s), true, 5, None, 0.0);
        }
        assert_eq!(action, VolAction::Raise(3.0), "the lowering is given back");
        assert_eq!(w.guard.disabled(), Some(GuardOff::Ineffective), "already off when the Raise comes out");
        move_volume(&b, &mut w, secs(t0, 10), action);
        assert_eq!(writes(), [-13.0, -10.0], "lowered, then given back although the guard is off");
        assert_eq!(record(&b), None);
        assert_eq!(w.note_text(secs(t0, 3600)).as_deref(), Some(MSG_SATURATING), "the saturation note stays for the run");
    }

    #[test]
    fn a_windows_volume_error_switches_the_guard_off_for_the_run_and_changes_nothing() {
        let b = volume_backend(hardware(-10.0)).with_volume(MicVolume { read: failing_read, write: fake_write });
        let (mut w, now) = (Watch::new(0.0), Instant::now());
        move_volume(&b, &mut w, now, VolAction::Lower(3.0));
        assert_eq!(w.guard.disabled(), Some(GuardOff::Error), "a read error");
        assert!(writes().is_empty() && record(&b).is_none());

        let b = volume_backend(hardware(-10.0)).with_volume(MicVolume { read: fake_read, write: failing_write });
        let mut w = Watch::new(0.0);
        move_volume(&b, &mut w, now, VolAction::Lower(3.0));
        assert_eq!(w.guard.disabled(), Some(GuardOff::Error), "a write error");
        assert!(record(&b).is_none() && w.guard.taken_db() == 0.0, "a lowering that did not happen is not recorded");
        assert!(w.note_text(now).is_none());
    }

    #[test]
    fn quitting_gives_the_volume_back_while_it_is_still_where_clarim_left_it() {
        let b = volume_backend(hardware(-13.0));
        set_record(&b, -10.0, 3.0);
        lock(&b.cfg).mic.voice_level_db = Some(-23.0);
        b.shutdown(); // idempotent, and the give-back is part of it
        assert_eq!(writes(), [-10.0]);
        assert_eq!(record(&b), None);
        assert_eq!(lock(&b.cfg).mic.voice_level_db, Some(-20.0), "the voice level goes back up with the volume");
        b.shutdown();
        assert_eq!(writes(), [-10.0], "a second quit changes nothing");
    }

    #[test]
    fn quitting_leaves_a_volume_the_user_moved_alone_and_only_drops_the_record() {
        let b = volume_backend(hardware(-5.0)); // not baseline - taken = -13
        set_record(&b, -10.0, 3.0);
        lock(&b.cfg).mic.voice_level_db = Some(-23.0);
        b.give_back_mic_volume();
        assert!(writes().is_empty());
        assert_eq!(record(&b), None);
        assert_eq!(lock(&b.cfg).mic.voice_level_db, Some(-23.0), "no volume change, no voice level change");
        // half a dB off is still "where Clarim left it"
        let b = volume_backend(hardware(-13.4));
        set_record(&b, -10.0, 3.0);
        b.give_back_mic_volume();
        assert_eq!(writes(), [-10.0]);
    }

    #[test]
    fn quitting_keeps_the_record_of_a_mic_that_is_gone_or_a_write_that_failed_and_never_acts_in_dry_run() {
        // not plugged in
        let b = volume_backend(hardware(-13.0));
        lock(&b.cfg).mic.mic_volume_taken = Some(taken_record("id-unplugged", -10.0, 3.0));
        b.give_back_mic_volume();
        assert!(writes().is_empty());
        assert!(record(&b).is_some(), "kept for the next time that mic is in use");
        // a failing write
        let b = volume_backend(hardware(-13.0)).with_volume(MicVolume { read: fake_read, write: failing_write });
        set_record(&b, -10.0, 3.0);
        b.give_back_mic_volume();
        assert!(record(&b).is_some(), "kept: the next quit tries again");
        // --dry-run never touches the machine
        let scan = Scan { inputs: vec![dev("Microfone (USB)", true)], outputs: vec![] };
        let flags = AgentFlags { dry_run: true, no_routing: false };
        let b = AppBackend::new(Config::default(), flags, scan, no_save).with_volume(MicVolume { read: fake_read, write: fake_write });
        FAKE_WRITES.with_borrow_mut(Vec::clear);
        set_record(&b, -10.0, 3.0);
        b.give_back_mic_volume();
        assert!(writes().is_empty() && record(&b).is_some());
    }

    #[test]
    fn check_mic_keeps_the_bookkeeping_of_the_lowered_volume_like_the_voice_level() {
        let taken = taken_record(MIC_ID, -10.0, 3.0);
        let stored = MicSettings { mic_volume_taken: Some(taken.clone()), ..MicSettings::default() };
        let posted = MicSettings { mic_volume_taken: None, leak_warnings: false, ..MicSettings::default() };
        let ok = check_mic(posted, &stored, false).expect("valid");
        assert_eq!(ok.mic_volume_taken, Some(taken), "a stale copy from the dashboard cannot erase it");
        assert!(!ok.leak_warnings, "the settings the dashboard owns do come through");
        let forged = MicSettings { mic_volume_taken: Some(taken_record("x", 0.0, 12.0)), ..MicSettings::default() };
        assert_eq!(check_mic(forged, &MicSettings::default(), false).expect("valid").mic_volume_taken, None);

        // and through the backend
        let b = safe_backend(no_save);
        set_record(&b, -10.0, 3.0);
        b.set_mic(MicSettings { enabled: false, auto_mic_volume: false, ..MicSettings::default() }).expect("set_mic");
        assert_eq!(record(&b), Some(taken_record(MIC_ID, -10.0, 3.0)));
        assert!(!lock(&b.cfg).mic.auto_mic_volume);
    }

    fn real(name: &str) -> DeviceInfo {
        dev(name, false)
    }

    fn ids(names: &[&str]) -> Vec<Option<String>> {
        names.iter().map(|n| Some(format!("id-{n}"))).collect()
    }

    #[test]
    fn references_are_the_channel_and_default_outputs_without_the_cable_the_monitor_or_virtual_devices() {
        let outputs = [
            real("Alto-falantes (Realtek)"),
            real("Fones (WZ-BT5.0)"),
            real("CABLE Input (VB-Audio Virtual Cable)"),
            real("Monitor (HDMI)"),
            real("Sonar Gaming (SteelSeries Sonar Virtual Audio Device)"),
        ];
        assert!(outputs[2].is_virtual && outputs[4].is_virtual && !outputs[1].is_virtual, "the fixtures are what the test needs");
        let speakers = "id-Alto-falantes (Realtek)";
        let own = ids(&["CABLE Input (VB-Audio Virtual Cable)", "Monitor (HDMI)"]);

        // Game on the default, Chat on the headset, Media on the cable, Aux on a virtual device
        let channels = [None, Some("id-Fones (WZ-BT5.0)".to_owned()), own[0].clone(), Some("id-Sonar Gaming (SteelSeries Sonar Virtual Audio Device)".to_owned())];
        let got = pick_references(&channels, Some(speakers), Some("id-Fones (WZ-BT5.0)"), &outputs, &own);
        assert_eq!(got, [speakers, "id-Fones (WZ-BT5.0)"], "channel order, the default for a channel on 'Windows default', no repeats");

        // the monitor is where the user hears their own voice: never a reference, even when a channel plays there
        let got = pick_references(&ids(&["Monitor (HDMI)"]), None, None, &outputs, &own);
        assert!(got.is_empty(), "{got:?}");

        // an id that is not a listed output (unplugged, stale config) is skipped; nothing to listen to is just empty
        let channels = [Some("id-gone".to_owned()), own[0].clone(), None, None];
        assert!(pick_references(&channels, None, None, &outputs, &own).is_empty());

        // ids are matched without regard to case and come out as the device list spells them
        let channels = [Some("ID-ALTO-FALANTES (REALTEK)".to_owned())];
        assert_eq!(pick_references(&channels, None, None, &outputs, &own), [speakers]);
    }

    #[test]
    fn references_are_capped_at_four_keeping_the_channels_first() {
        let names = ["A (1)", "B (2)", "C (3)", "D (4)", "E (5)", "F (6)"];
        let outputs: Vec<DeviceInfo> = names.iter().map(|n| real(n)).collect();
        let got = pick_references(&ids(&names[..4]), Some("id-E (5)"), Some("id-F (6)"), &outputs, &[]);
        assert_eq!(got, ["id-A (1)", "id-B (2)", "id-C (3)", "id-D (4)"], "the defaults come after the channels and are cut");
        let got = pick_references(&[None, None, None, None], Some("id-E (5)"), Some("id-F (6)"), &outputs, &[]);
        assert_eq!(got, ["id-E (5)", "id-F (6)"], "channels on 'Windows default' and both defaults are one device each");
    }

    #[test]
    fn plan_volume_covers_what_a_lowering_and_a_giving_back_come_to() {
        let rec = taken_record(MIC_ID, -10.0, 3.0);
        // a lowering keeps an existing baseline; a first one makes the record
        let plan = plan_volume(VolAction::Lower(3.0), &hardware(-13.0), Some(&rec), MIC_ID);
        assert_eq!(plan, VolPlan::Set { target: -16.0, record: rec.clone() });
        let plan = plan_volume(VolAction::Lower(3.0), &hardware(-10.0), None, MIC_ID);
        assert_eq!(plan, VolPlan::Set { target: -13.0, record: taken_record(MIC_ID, -10.0, 0.0) });
        let plan = plan_volume(VolAction::Lower(3.0), &hardware(-29.0), None, MIC_ID);
        assert!(matches!(plan, VolPlan::Set { target, .. } if target == -30.0), "never below the range: {plan:?}");
        assert_eq!(plan_volume(VolAction::Lower(3.0), &hardware(-30.0), Some(&rec), MIC_ID), VolPlan::NoHardware, "already at the floor");
        // giving back stops at the baseline, also when asked for more than is left
        let plan = plan_volume(VolAction::Raise(3.0), &hardware(-11.5), Some(&taken_record(MIC_ID, -10.0, 1.5)), MIC_ID);
        assert!(matches!(plan, VolPlan::Set { target, .. } if target == -10.0), "{plan:?}");
        // nothing to give back: no record, or the volume is at the baseline already (the user raised it)
        assert_eq!(plan_volume(VolAction::Raise(3.0), &hardware(-13.0), None, MIC_ID), VolPlan::Forget);
        assert_eq!(plan_volume(VolAction::Raise(3.0), &hardware(-9.0), Some(&rec), MIC_ID), VolPlan::Forget);
        assert_eq!(plan_volume(VolAction::None, &hardware(-9.0), Some(&rec), MIC_ID), VolPlan::Skip);
        // giving back works on any hardware state: the lowering already proved the control exists
        let soft = EndpointVolume { hardware: false, ..hardware(-13.0) };
        assert!(matches!(plan_volume(VolAction::Raise(3.0), &soft, Some(&rec), MIC_ID), VolPlan::Set { .. }));
        assert_eq!(plan_volume(VolAction::Lower(3.0), &soft, Some(&rec), MIC_ID), VolPlan::NoHardware);
    }

    #[test]
    fn the_guard_runs_only_in_a_call_with_both_settings_on_and_a_record_that_fits_the_mic_in_use() {
        let on = MicSettings::default();
        assert!(guard_active(&on, true, true, Some(MIC_ID)));
        assert!(!guard_active(&on, false, true, Some(MIC_ID)), "no call");
        assert!(!guard_active(&on, true, false, Some(MIC_ID)), "mic not running");
        assert!(!guard_active(&on, true, true, None), "no mic in use");
        assert!(!guard_active(&MicSettings { auto_level: false, ..on.clone() }, true, true, Some(MIC_ID)));
        assert!(!guard_active(&MicSettings { auto_mic_volume: false, ..on.clone() }, true, true, Some(MIC_ID)));
        let lowered = MicSettings { mic_volume_taken: Some(taken_record(MIC_ID, -10.0, 3.0)), ..on };
        assert!(guard_active(&lowered, true, true, Some(MIC_ID)), "the record is about this mic");
        assert!(!guard_active(&lowered, true, true, Some("id-other mic")), "the record belongs to another mic");
    }

    #[test]
    fn clip_counts_are_deltas_and_a_lower_count_is_a_restarted_engine() {
        assert_eq!(clip_delta(10, 14), 4);
        assert_eq!(clip_delta(7, 7), 0);
        assert_eq!(clip_delta(500, 3), 3, "the engine restarted: its 3 clipped frames are all new");
        assert_eq!(clip_delta(500, 0), 0, "stopped");
        assert_eq!(clip_delta(0, 9), 9);
    }

    #[test]
    fn the_tooltip_and_the_balloons_say_what_the_design_says() {
        assert_eq!(tip_for(false, None), "Clarim");
        assert_eq!(tip_for(true, None), "Clarim · em chamada");
        assert_eq!(tip_for(true, Some(LeakKind::Noise)), "Clarim · som vazando na chamada");
        assert_eq!(tip_for(false, Some(LeakKind::Echo)), "Clarim", "an alert never outlives the call");
        assert_eq!(
            leak_balloon(LeakKind::Echo),
            ("Som vazando na chamada", "O som das caixas está entrando no seu microfone. Abaixe o volume ou use fone.")
        );
        assert_eq!(
            leak_balloon(LeakKind::Noise),
            ("Ruído passando na chamada", "Tem ruído de fundo passando pelo filtro. Aproxime o microfone ou ligue o Foco na voz.")
        );
        assert_eq!(BALLOON_LOWERED, ("Microfone estourando", "Baixei o volume do microfone no Windows em 3 dB."));
        assert_eq!((fmt_db(3.0), fmt_db(1.5), fmt_db(0.0), fmt_db(2.96)), ("3".into(), "1,5".into(), "0".into(), "3".into()));
    }

    #[test]
    fn the_volume_note_shows_for_a_minute_unless_it_is_the_one_that_stays() {
        let (mut w, t0) = (Watch::new(0.0), Instant::now());
        assert_eq!(w.note_text(t0), None);
        w.note_for_a_minute("nota".into(), t0);
        assert_eq!(w.note_text(secs(t0, 59)).as_deref(), Some("nota"));
        assert_eq!(w.note_text(secs(t0, 60)), None, "gone after 60 s");
        assert_eq!(w.note_text(secs(t0, 61)), None, "and does not come back");
        w.note_for_the_run("fica");
        assert_eq!(w.note_text(secs(t0, 86_400)).as_deref(), Some("fica"));
    }

    fn call_backend() -> AppBackend {
        let b = safe_backend(no_save);
        lock(&b.devices).inputs.push(dev("CABLE Output (VB-Audio Virtual Cable)", false));
        b
    }

    fn running() -> MicStatus {
        MicStatus { running: true, output_name: Some(CABLE_RENDER.into()), ..MicStatus::default() }
    }

    fn capture(exe: &str, active: bool) -> CaptureSession {
        CaptureSession { pid: 4, exe: exe.into(), device_id: CABLE_CAPTURE_ID.into(), active }
    }

    /// One `watch_call` tick on the idle test engine.
    fn watch_tick(b: &AppBackend, w: &mut Watch, want: &MicSettings, st: &MicStatus, caps: &[CaptureSession], now: Instant) {
        let mut eng = lock(&b.mic);
        b.watch_call(&mut eng, w, want, st, caps, now);
    }

    #[test]
    fn a_call_app_on_the_virtual_mic_shows_in_every_status_even_the_cached_one_and_ends_after_the_hangover() {
        let b = call_backend();
        let (mut w, t0, want) = (Watch::new(0.0), Instant::now(), MicSettings::default());
        watch_tick(&b, &mut w, &want, &running(), &[capture("discord.exe", true), capture("obs64.exe", true)], t0);
        for (what, mic) in [("snapshot", b.snapshot().mic), ("meters", b.meters().mic)] {
            assert!(mic.in_call, "{what}");
            assert_eq!(mic.call_apps, ["discord.exe"], "{what}: OBS is not a call");
        }
        assert_eq!(w.tip, "Clarim · em chamada");
        {
            // `apply` holds the engine: the cached engine status is served, with the overlay on top
            let _held = lock(&b.mic);
            let mic = b.mic_status();
            assert!(mic.in_call && mic.call_apps == ["discord.exe"], "cached path: {mic:?}");
        }
        watch_tick(&b, &mut w, &want, &running(), &[], secs(t0, 7));
        assert!(b.snapshot().mic.in_call, "still in the 8 s hangover");
        watch_tick(&b, &mut w, &want, &running(), &[], secs(t0, 8));
        let mic = b.snapshot().mic;
        assert!(!mic.in_call && mic.call_apps.is_empty() && mic.leak.is_none(), "{mic:?}");
        assert_eq!(w.tip, "Clarim");
    }

    #[test]
    fn a_session_on_another_endpoint_or_a_paused_mic_is_no_call() {
        let b = call_backend();
        let (mut w, t0, want) = (Watch::new(0.0), Instant::now(), MicSettings::default());
        let elsewhere = CaptureSession { device_id: "id-Microfone (USB)".into(), ..capture("discord.exe", true) };
        watch_tick(&b, &mut w, &want, &running(), &[elsewhere, capture("zoom.exe", false)], t0);
        assert!(!b.snapshot().mic.in_call, "only an active session on the virtual mic's capture side counts");
        // no virtual mic in use (mic paused: the engine reports no output name): never a call
        watch_tick(&b, &mut w, &want, &MicStatus::default(), &[capture("discord.exe", true)], secs(t0, 1));
        assert!(!b.snapshot().mic.in_call);
    }

    #[test]
    fn a_steady_echo_raises_the_alert_and_the_tooltip_and_turning_the_warnings_off_clears_both() {
        let b = call_backend();
        let (mut w, t0, want) = (Watch::new(0.0), Instant::now(), MicSettings::default());
        let st = MicStatus {
            references: 1,
            echo_score: 0.8,
            echo_db: Some(-45.0),
            echo_lag_ms: 250.0,
            noise_db: Some(-70.0),
            ..running()
        };
        let caps = [capture("discord.exe", true)];
        for s in 0..4 {
            watch_tick(&b, &mut w, &want, &st, &caps, secs(t0, s));
            assert_eq!(b.snapshot().mic.leak, None, "tick {s}: the lag needs a previous tick to hold still against");
        }
        watch_tick(&b, &mut w, &want, &st, &caps, secs(t0, 4));
        assert_eq!(b.snapshot().mic.leak, Some(LeakKind::Echo));
        assert_eq!(w.tip, "Clarim · som vazando na chamada");
        // the balloon cannot be delivered here (no tray), so it stays wanted and the alert stays
        watch_tick(&b, &mut w, &want, &st, &caps, secs(t0, 5));
        assert_eq!(b.snapshot().mic.leak, Some(LeakKind::Echo));

        let off = MicSettings { leak_warnings: false, ..want };
        watch_tick(&b, &mut w, &off, &st, &caps, secs(t0, 6));
        let mic = b.snapshot().mic;
        assert!(mic.in_call && mic.leak.is_none(), "the call stays, the alert goes: {mic:?}");
        assert_eq!(w.tip, "Clarim · em chamada");
    }

    #[test]
    fn the_volume_note_and_the_quiet_flag_reach_the_dashboard_and_the_note_expires() {
        let b = call_backend();
        let (mut w, t0, want) = (Watch::new(0.0), Instant::now(), MicSettings::default());
        w.note_for_a_minute("Devolvi 3 dB.".into(), t0);
        watch_tick(&b, &mut w, &want, &running(), &[], t0);
        let mic = b.snapshot().mic;
        assert_eq!(mic.mic_volume_note.as_deref(), Some("Devolvi 3 dB."));
        assert!(!mic.mic_too_quiet);
        watch_tick(&b, &mut w, &want, &running(), &[], secs(t0, 60));
        assert_eq!(b.snapshot().mic.mic_volume_note, None);
    }

    #[test]
    fn the_per_call_memory_is_forgotten_when_the_call_ends() {
        let b = call_backend();
        let (mut w, t0, want) = (Watch::new(0.0), Instant::now(), MicSettings::default());
        watch_tick(&b, &mut w, &want, &running(), &[capture("discord.exe", true)], t0);
        w.refs_off_logged = true;
        w.lowered_balloon_told = true;
        w.lowered_balloon_due = true;
        watch_tick(&b, &mut w, &want, &running(), &[], secs(t0, 20));
        assert!(!w.refs_off_logged && !w.lowered_balloon_told && !w.lowered_balloon_due, "the next call starts clean");
    }

    #[test]
    fn the_housekeeping_tick_on_an_idle_mic_reports_no_call() {
        let b = safe_backend(no_save);
        let (mut retry, mut w) = (Retry::new(), Watch::new(0.0));
        b.tick_mic(&mut retry, &mut w);
        let mic = b.snapshot().mic;
        assert!(!mic.in_call && mic.call_apps.is_empty() && mic.leak.is_none() && !mic.mic_too_quiet && mic.mic_volume_note.is_none());
        assert_eq!(w.tip, "Clarim");
    }
}
