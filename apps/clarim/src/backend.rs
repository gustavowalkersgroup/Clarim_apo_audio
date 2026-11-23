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
//! (`devices`, `eq`). `devices`, `last_mic`, `eq`, `routing` and `housekeeper` are leaf locks (`routing` is
//! also taken under `cfg`, only for a non-blocking send, so updates reach it in order).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use clarim_audio::devices::is_denied;
use clarim_audio::MicEngine;
use clarim_core::{
    endpoint_guid, AppSession, Backend, ChannelId, ChannelSettings, Config, DeviceInfo, EqApoStatus, Meters, MicSettings,
    MicStatus, OutputEq, Snapshot, EQ_TONE_RANGE_DB,
};
use clarim_routing::{RoutingConfig, RoutingEngine, UNKNOWN_EXE};
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::eqapo::{self, Service};
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
/// the dashboard can never undo a fresh calibration.
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

pub struct AppBackend {
    flags: AgentFlags,
    save: SaveFn,
    eq_write: EqWriteFn,
    cfg: Mutex<Config>,
    devices: Mutex<Scan>,
    mic: Mutex<MicEngine>,
    /// Last status read, served while `apply` holds the engine.
    last_mic: Mutex<MicStatus>,
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

    /// Mic status. Never blocks on `apply`: while it holds the engine, the previous status is served.
    fn mic_status(&self) -> MicStatus {
        let mut eng = match self.mic.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return lock(&self.last_mic).clone(),
        };
        // Status first, then fold: whoever sees `calibrating == false` has the result already
        // saved (the engine publishes the result before it clears the flag).
        let st = eng.status();
        self.fold_calibration(&mut eng);
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

    /// Every second: fold a calibration result, retry the mic if it should be running and is not;
    /// every 5 s refresh the device lists; every 10 s look at Equalizer APO again.
    fn housekeep(&self) {
        let _ = clarim_audio::init_com();
        let (mut last_scan, mut last_apo) = (Instant::now(), Instant::now());
        let mut retry = Retry::new();
        self.write_stale_eq(); // the first write of the saved equalizers, off the start-up path
        while matches!(self.stop.1.recv_timeout(TICK), Err(crossbeam_channel::RecvTimeoutError::Timeout)) {
            self.tick_mic(&mut retry);
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

    fn tick_mic(&self, retry: &mut Retry) {
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

    /// Stops housekeeping, the mic engine and the routing worker (routes already written stay in
    /// Windows, see `clarim reset-routing`). Idempotent.
    pub fn shutdown(&self) {
        let _ = self.stop.0.try_send(());
        if let Some(h) = lock(&self.housekeeper).take() {
            let _ = h.join();
        }
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
}
