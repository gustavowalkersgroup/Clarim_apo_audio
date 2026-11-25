//! Per-app audio session enumeration and Windows per-app default endpoint routing.
//!
//! One worker thread (COM MTA) owns every COM object; the rest of the program only sees plain data
//! copied out through a mutex. With `RoutingConfig::enabled == false` the worker observes only: it
//! never calls SetPersistedDefaultAudioEndpoint, SetMasterVolume or SetMute (except once, when
//! routing is switched off, to release what it wrote).

mod policy;
mod sessions;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use clarim_core::{AppSession, CaptureSession, ChannelId, ChannelSettings};
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, ISimpleAudioVolume};

use policy::{Policy, ROLE_COMMUNICATIONS, ROLE_MULTIMEDIA};
use sessions::{CaptureInfo, ComGuard, Scan, SessionInfo};

pub use sessions::UNKNOWN_EXE;

/// Writes allowed per (pid, desired device) so a process that refuses never causes a write loop.
const MAX_ROUTE_ATTEMPTS: u8 = 3;
const PEAK_PERIOD: Duration = Duration::from_millis(100);
const SCAN_PERIOD: Duration = Duration::from_secs(1);
/// Nothing is written while routing is off, so the scan (list + call detection) can be lazier.
const SCAN_PERIOD_OFF: Duration = Duration::from_secs(2);
/// Re-scan soon after a routing write so the moved session gets its channel volume quickly.
const RESCAN_AFTER_ROUTE: Duration = Duration::from_millis(250);
const VOLUME_EPSILON: f32 = 0.001;
/// A Midia app that captured a microphone stays on Chat this long after the capture was last seen.
const CALL_HANGOVER: Duration = Duration::from_secs(20);
/// Nobody asked for sessions or peaks for this long = dashboard closed: skip the 10 Hz meters.
const IDLE_AFTER: Duration = Duration::from_secs(3);
const IDLE_PERIOD: Duration = Duration::from_millis(500);
/// Volume and mute as Windows creates them: what a session we never touched is assumed to have.
const UNTOUCHED: Level = Level { vol: 1.0, muted: false };

/// Inputs of the routing worker. Cheap to clone, sent to the worker on every change.
#[derive(Clone)]
pub struct RoutingConfig {
    pub channels: BTreeMap<ChannelId, ChannelSettings>,
    pub app_rules: BTreeMap<String, ChannelId>,
    pub default_channel: ChannelId,
    /// false = enumerate/meter only, never call SetPersistedDefaultAudioEndpoint / SetMasterVolume / SetMute.
    /// The one exception is the moment it goes true -> false: what this run wrote is released once.
    pub enabled: bool,
    /// Midia apps that capture a microphone play on Chat until 20 s after the call (`Config::call_to_chat`).
    pub call_to_chat: bool,
}

impl RoutingConfig {
    /// (channel, has_rule). Rule keys are lowercase exe file names; lookup is case-insensitive.
    fn resolve(&self, exe: &str) -> (ChannelId, bool) {
        match self.app_rules.get(&exe.to_lowercase()) {
            Some(&ch) => (ch, true),
            None => (self.default_channel, false),
        }
    }

    /// Endpoint a channel wants its apps on: the configured one, but only if it is currently an
    /// active render endpoint. Anything else is `None` (Windows default): an absent id must never
    /// reach the policy API, it can silence the app.
    fn desired_device<'a>(&self, ch: ChannelId, active: &'a [String]) -> Option<&'a str> {
        let want = self.channels.get(&ch)?.device_id.as_deref()?;
        active.iter().find(|id| id.eq_ignore_ascii_case(want)).map(String::as_str)
    }

    fn desired_level(&self, ch: ChannelId) -> Level {
        self.channels.get(&ch).map_or(UNTOUCHED, |s| Level {
            vol: if s.volume.is_finite() { s.volume.clamp(0.0, 1.0) } else { 1.0 },
            muted: s.muted,
        })
    }

    /// (effective channel, has_rule, in_call): `resolve`, then Midia becomes Chat while the app is in a call.
    fn effective(&self, exe: &str, captures: &Captures, now: Instant) -> (ChannelId, bool, bool) {
        let (rule, has_rule) = self.resolve(exe);
        let (channel, in_call) = call_channel(rule, captures.get(exe).copied(), now, self.call_to_chat);
        (channel, has_rule, in_call)
    }
}

/// exe (lowercase) -> last time an active microphone session of that exe was seen.
type Captures = HashMap<String, Instant>;

/// "Call follows Chat": a Midia app (browser tab on Meet/Teams/Discord web) that captured a microphone
/// at most [`CALL_HANGOVER`] ago plays on Chat. Other channels are never promoted (a game with in-game
/// voice stays on Jogo). Returns (effective channel, in_call).
fn call_channel(rule: ChannelId, last_capture: Option<Instant>, now: Instant, call_to_chat: bool) -> (ChannelId, bool) {
    let in_call = call_to_chat
        && rule == ChannelId::Media
        && last_capture.is_some_and(|t| now.saturating_duration_since(t) < CALL_HANGOVER);
    (if in_call { ChannelId::Chat } else { rule }, in_call)
}

/// Per-pid routing bookkeeping: the target we are driving towards and how many writes it cost.
#[derive(Default)]
struct RouteState {
    target: Option<Option<String>>,
    attempts: u8,
    /// The attempt cap was already reported for `target`.
    warned: bool,
}

/// What `route_step` wants done for one pid.
#[derive(PartialEq, Eq, Debug)]
enum RouteAction {
    /// Nothing to write (already there, or the cap was reached and reported before).
    Keep,
    Write,
    /// The cap blocks a still-needed write; returned once per target so the caller logs it once.
    GiveUp,
}

fn same_device(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// The persisted device to compare with `desired`: the Multimedia one, or the Communications one when
/// Multimedia already matches (Console is always written together with Multimedia, so it is not read).
fn mismatched<'a>(desired: Option<&str>, multimedia: Option<&'a str>, communications: Option<&'a str>) -> Option<&'a str> {
    if same_device(desired, multimedia) {
        communications
    } else {
        multimedia
    }
}

/// Decide whether to write the routing rule: `Write` = call Set for all three roles now.
/// `current` is the persisted device read back from Windows (see [`mismatched`]). A new `desired`
/// restarts the budget; matching does not refund it, so something that keeps flipping the rule back
/// cannot make us loop.
fn route_step(st: &mut RouteState, desired: Option<&str>, current: Option<&str>) -> RouteAction {
    if st.target.as_ref().map(|t| t.as_deref()).is_none_or(|t| !same_device(t, desired)) {
        st.target = Some(desired.map(str::to_owned));
        st.attempts = 0;
        st.warned = false;
    }
    if same_device(desired, current) {
        return RouteAction::Keep;
    }
    if st.attempts >= MAX_ROUTE_ATTEMPTS {
        return if std::mem::replace(&mut st.warned, true) { RouteAction::Keep } else { RouteAction::GiveUp };
    }
    st.attempts += 1;
    RouteAction::Write
}

/// Volume and mute of one session, as we want it or last set it.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Level {
    vol: f32,
    muted: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Writes {
    volume: bool,
    mute: bool,
}

/// Which of SetMasterVolume / SetMute to call. Change-only against what WE last applied to this
/// session, so a tweak in the Windows mixer survives until the channel itself changes. A session
/// we have never touched is compared with its current state instead (read lazily via `current`),
/// except that a default channel level (100 %, unmuted) never touches a session at first sight: the
/// app keeps whatever volume the user or Windows gave it. That shortcut is for apps Clarim has not
/// written in this run (`exe_touched` false): Windows keeps the level per app and starts every new
/// session of it there, so a session of an app Clarim did write (a new tab's audio, a restarted app, a
/// promoted call) may start at a stale level of ours and must be compared and corrected.
fn level_writes(last: Option<Level>, want: Level, exe_touched: bool, current: impl FnOnce() -> Option<Level>) -> Writes {
    if last.is_none() && want == UNTOUCHED && !exe_touched {
        return Writes::default();
    }
    match last.or_else(current) {
        None => Writes { volume: true, mute: true },
        Some(base) => Writes { volume: (base.vol - want.vol).abs() > VOLUME_EPSILON, mute: base.muted != want.muted },
    }
}

fn read_level(vol: &ISimpleAudioVolume) -> Option<Level> {
    // SAFETY: `vol` is a live ISimpleAudioVolume used on the thread that created it (COM initialised).
    unsafe { Some(Level { vol: vol.GetMasterVolume().ok()?, muted: vol.GetMute().ok()?.as_bool() }) }
}

fn write_level(vol: &ISimpleAudioVolume, w: &Writes, want: Level) -> windows::core::Result<()> {
    // SAFETY: as in `read_level`; a null event context is allowed.
    unsafe {
        if w.volume {
            vol.SetMasterVolume(want.vol, std::ptr::null())?;
        }
        if w.mute {
            vol.SetMute(want.muted, std::ptr::null())?;
        }
    }
    Ok(())
}

/// What Clarim changed in this run, so turning routing off can undo exactly that (and nothing else).
///
/// Keyed by exe, not by pid or session: Windows stores a per-app route and a per-app volume under the exe
/// path, so a restarted app (new pid), a respawned browser audio process or a new session of the same app
/// starts with what we wrote and never shows up as a write of its own. Never pruned while the engine runs
/// (an app whose audio paused still carries our route); only `release` reads and empties it.
#[derive(Default)]
struct Footprint {
    /// exes whose per-app route we pointed at a device.
    routed: HashSet<String>,
    /// exe -> which of volume / mute we wrote.
    levels: HashMap<String, Writes>,
}

impl Footprint {
    fn note_route(&mut self, exe: &str, device: Option<&str>) {
        if device.is_some() {
            self.routed.insert(exe.to_owned());
        } else {
            // We cleared the rule: nothing of ours is left on this exe.
            self.routed.remove(exe);
        }
    }

    fn note_levels(&mut self, exe: &str, w: &Writes) {
        if w.volume || w.mute {
            let e = self.levels.entry(exe.to_owned()).or_default();
            e.volume |= w.volume;
            e.mute |= w.mute;
        }
    }

    /// Pids (deduplicated, ascending) among `live` (pid, exe) whose exe carries a route of ours. Clearing
    /// through each of them is what undoes the per-exe rule, whichever pid it was written through.
    fn routed_pids<'a>(&self, live: impl Iterator<Item = (u32, &'a str)>) -> Vec<u32> {
        let pids: std::collections::BTreeSet<u32> = live.filter(|(_, exe)| self.routed.contains(*exe)).map(|(pid, _)| pid).collect();
        pids.into_iter().collect()
    }
}

/// One entry per pid (the active session wins), sorted by exe then pid. `AppSession::channel` is the
/// EFFECTIVE channel (Chat while `in_call`).
fn app_sessions<'a>(
    infos: impl Iterator<Item = &'a SessionInfo>,
    cfg: &RoutingConfig,
    captures: &Captures,
    now: Instant,
) -> Vec<AppSession> {
    let mut by_pid: BTreeMap<u32, AppSession> = BTreeMap::new();
    for s in infos {
        let (channel, has_rule, in_call) = cfg.effective(&s.exe, captures, now);
        let cand = AppSession {
            pid: s.pid,
            exe: s.exe.clone(),
            name: s.name.clone(),
            channel,
            has_rule,
            in_call,
            device_id: s.device_id.clone(),
            active: s.active,
            peak: s.peak,
        };
        match by_pid.entry(s.pid) {
            Entry::Vacant(v) => {
                v.insert(cand);
            }
            Entry::Occupied(mut o) => {
                if cand.active && !o.get().active {
                    o.insert(cand);
                }
            }
        }
    }
    let mut out: Vec<AppSession> = by_pid.into_values().collect();
    out.sort_by(|a, b| a.exe.cmp(&b.exe).then(a.pid.cmp(&b.pid)));
    out
}

#[derive(Default)]
struct Shared {
    sessions: Vec<AppSession>,
    peaks: Vec<(u32, f32)>,
    /// Every app capture session of the last scan (all endpoints, any state). Empty when that scan failed.
    captures: Vec<CaptureSession>,
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

enum Msg {
    Update(RoutingConfig),
    Stop,
}

/// When the dashboard last asked for data. Lets an unwatched engine skip the 10 Hz meters.
struct Poll {
    start: Instant,
    /// Milliseconds since `start` of the last `sessions()` / `peaks()` call (0 = engine start).
    last_polled: AtomicU64,
}

impl Poll {
    fn new() -> Self {
        Self { start: Instant::now(), last_polled: AtomicU64::new(0) }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn touch(&self) {
        self.last_polled.store(self.now_ms(), Ordering::Relaxed);
    }

    fn recent(&self) -> bool {
        polled_recently(self.last_polled.load(Ordering::Relaxed), self.now_ms())
    }
}

fn polled_recently(last_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_ms) <= IDLE_AFTER.as_millis() as u64
}

/// Owns the worker thread (COM MTA).
pub struct RoutingEngine {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Shared>>,
    poll: Arc<Poll>,
    worker: Option<JoinHandle<()>>,
}

impl RoutingEngine {
    pub fn start(cfg: RoutingConfig) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let poll = Arc::new(Poll::new());
        let (worker_shared, worker_poll) = (Arc::clone(&shared), Arc::clone(&poll));
        let worker = std::thread::Builder::new()
            .name("clarim-routing".into())
            .spawn(move || run(rx, worker_shared, worker_poll, cfg))
            .map_err(|e| log::error!("routing: cannot start worker thread: {e}"))
            .ok();
        Self { tx, shared, poll, worker }
    }

    /// Non-blocking; the worker re-enumerates and re-applies immediately.
    pub fn update(&self, cfg: RoutingConfig) {
        let _ = self.tx.send(Msg::Update(cfg));
    }

    /// Last enumeration (at most 1 s old; 2 s while routing is off), with peaks refreshed at 10 Hz
    /// while somebody keeps polling (the meters idle after 3 s without a call to this or `peaks`).
    pub fn sessions(&self) -> Vec<AppSession> {
        self.poll.touch();
        let g = lock(&self.shared);
        let mut out = g.sessions.clone();
        for s in &mut out {
            if let Some(&(_, p)) = g.peaks.iter().find(|(pid, _)| *pid == s.pid) {
                s.peak = p;
            }
        }
        out
    }

    /// (pid, peak), at most 100 ms old while polled.
    pub fn peaks(&self) -> Vec<(u32, f32)> {
        self.poll.touch();
        lock(&self.shared).peaks.clone()
    }

    /// Every app capture (microphone) session of the last scan, same age as `sessions`. Empty while Windows
    /// Audio does not answer. Does NOT count as the dashboard polling: the call watcher asks every second
    /// and must not keep the 10 Hz meters awake.
    pub fn captures(&self) -> Vec<CaptureSession> {
        lock(&self.shared).captures.clone()
    }

    /// Stops the worker and waits for it (same as dropping the engine).
    pub fn stop(self) {}
}

impl Drop for RoutingEngine {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// What one scan says about the link to Windows Audio.
struct ScanOutcome {
    /// A routing rule was written (re-scan soon, to give the moved session its channel volume).
    wrote_routes: bool,
    /// The scan could talk to Windows Audio (see [`scan_healthy`]).
    healthy: bool,
}

/// Did a scan reach Windows Audio? No: the enumeration failed (`enumerated` false), no playback endpoint was
/// listed (every endpoint of a PC is "gone" while Audiosrv is down), or every per-app routing call failed
/// (the policy factory kept from before a Windows Audio restart answers nothing). A PC whose endpoints are
/// all disabled looks the same and is rebuilt now and then at no cost.
fn scan_healthy(enumerated: bool, endpoints: usize, policy_calls: u32, policy_errors: u32) -> bool {
    enumerated && endpoints > 0 && (policy_calls == 0 || policy_errors < policy_calls)
}

/// What to do after a scan, from the history of scans.
#[derive(Debug, PartialEq, Eq)]
enum Health {
    Fine,
    /// Not reachable yet, nothing to do but wait.
    Waiting,
    /// Rebuild the enumerator and the policy factory now.
    Rebuild,
    /// Reachable again after a bad spell.
    Recovered,
}

/// Counts consecutive bad scans: a rebuild on the 3rd, then on every 5th after that, so a Windows Audio
/// that takes a while to come back is retried without a rebuild per second.
#[derive(Default)]
struct HealthTracker {
    bad_in_a_row: u32,
}

impl HealthTracker {
    fn note(&mut self, healthy: bool) -> Health {
        if healthy {
            return if std::mem::take(&mut self.bad_in_a_row) >= 3 { Health::Recovered } else { Health::Fine };
        }
        self.bad_in_a_row = self.bad_in_a_row.saturating_add(1);
        let n = self.bad_in_a_row;
        if n == 3 || (n > 3 && (n - 3).is_multiple_of(5)) {
            Health::Rebuild
        } else {
            Health::Waiting
        }
    }
}

/// Worker thread body. Locals drop in reverse order: the worker (all its COM objects) goes before
/// the enumerator and the COM guard.
fn run(rx: mpsc::Receiver<Msg>, shared: Arc<Mutex<Shared>>, poll: Arc<Poll>, cfg: RoutingConfig) {
    let _com = match ComGuard::init() {
        Ok(g) => g,
        Err(e) => return log::error!("routing: {e}"),
    };
    let mut en = match sessions::device_enumerator() {
        Ok(en) => en,
        Err(e) => return log::error!("routing: {e}"),
    };
    let mut w = Worker::new(cfg, shared);
    let mut health = HealthTracker::default();
    let mut next_scan = Instant::now();
    loop {
        if Instant::now() >= next_scan {
            let outcome = w.scan(&en);
            // Windows Audio restarted (the user pressed "Reiniciar audio", or it crashed): the enumerator and the
            // policy factory made before may be dead for good, so new ones are made, and routes and levels are
            // re-checked from scratch.
            match health.note(outcome.healthy) {
                Health::Rebuild => {
                    log::info!("routing: Windows Audio is not answering; reconnecting");
                    match sessions::device_enumerator() {
                        Ok(fresh) => en = fresh,
                        Err(e) => log::debug!("routing: {e}"),
                    }
                    w.reconnect();
                }
                Health::Recovered => log::info!("routing: Windows Audio is back"),
                Health::Fine | Health::Waiting => {}
            }
            let wrote_routes = outcome.wrote_routes;
            let period = if wrote_routes {
                RESCAN_AFTER_ROUTE
            } else if w.cfg.enabled {
                SCAN_PERIOD
            } else {
                SCAN_PERIOD_OFF
            };
            next_scan = Instant::now() + period;
        }
        let watched = poll.recent();
        if watched {
            w.publish_peaks();
        }
        // Wake for the next meter tick (or the slower idle tick), but never sleep past the next scan.
        let tick = if watched { PEAK_PERIOD } else { IDLE_PERIOD };
        let first = rx.recv_timeout(tick.min(next_scan.saturating_duration_since(Instant::now())));
        let mut stop = matches!(first, Err(RecvTimeoutError::Disconnected));
        for msg in first.into_iter().chain(rx.try_iter()) {
            match msg {
                Msg::Update(c) => {
                    let was_enabled = w.cfg.enabled;
                    w.cfg = c;
                    if was_enabled && !w.cfg.enabled {
                        w.release();
                    }
                    next_scan = Instant::now();
                }
                Msg::Stop => stop = true,
            }
        }
        if stop {
            break;
        }
    }
}

struct Worker {
    cfg: RoutingConfig,
    shared: Arc<Mutex<Shared>>,
    /// None = unsupported Windows build / activation failed: volume-only.
    policy: Option<Policy>,
    routes: HashMap<u32, RouteState>,
    /// session key -> level we last applied
    applied: HashMap<String, Level>,
    /// What we changed in this run, undone when routing is turned off.
    footprint: Footprint,
    /// Exes whose volume or mute we wrote in this run. Kept for the whole run (not pruned with the
    /// session, not cleared by `release`): their next session may start at a level of ours that Windows
    /// remembered, so it is compared with the wanted one instead of being left alone (see `level_writes`).
    touched_exes: HashSet<String>,
    exes: HashMap<u32, String>,
    /// exe -> last active microphone session (call detection, see [`call_channel`]).
    captures: Captures,
    /// COM objects of the last scan, used for the 10 Hz peaks.
    live: Vec<sessions::Live>,
    own_pid: u32,
    /// Per-app routing lookups tried, and how many of them failed, in the scan in progress (see `scan_healthy`).
    policy_calls: u32,
    policy_errors: u32,
}

impl Worker {
    fn new(cfg: RoutingConfig, shared: Arc<Mutex<Shared>>) -> Self {
        let policy = match Policy::new() {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!("routing: per-app output routing unavailable, volume control only: {e}");
                None
            }
        };
        Self {
            cfg,
            shared,
            policy,
            routes: HashMap::new(),
            applied: HashMap::new(),
            footprint: Footprint::default(),
            touched_exes: HashSet::new(),
            exes: HashMap::new(),
            captures: Captures::new(),
            live: Vec::new(),
            own_pid: std::process::id(),
            policy_calls: 0,
            policy_errors: 0,
        }
    }

    /// Makes new COM objects after Windows Audio went away: a new policy factory (the old one is kept when a
    /// new one cannot be made yet) and a clean slate for routes and levels, so everything is checked again.
    fn reconnect(&mut self) {
        match Policy::new() {
            Ok(p) => self.policy = Some(p),
            Err(e) => log::debug!("routing: policy factory not rebuilt: {e}"),
        }
        self.routes.clear();
        self.applied.clear();
        self.live.clear();
    }

    /// Enumerate, apply (when enabled), publish.
    fn scan(&mut self, en: &IMMDeviceEnumerator) -> ScanOutcome {
        let scan = match sessions::enumerate(en, self.own_pid, &mut self.exes) {
            Ok(s) => s,
            Err(e) => {
                log::debug!("routing: enumeration failed: {e}");
                // Stale captures would keep a call alive while Windows Audio is down.
                lock(&self.shared).captures.clear();
                return ScanOutcome { wrote_routes: false, healthy: false };
            }
        };
        self.policy_calls = 0;
        self.policy_errors = 0;
        let endpoints = scan.endpoints.len();
        let now = Instant::now();
        // Always listed (the call watcher reads them whatever `call_to_chat` says); promotion keeps its gating.
        let caps = capture_list(en, self.own_pid, &mut self.exes);
        let render_pids: HashSet<u32> = scan.sessions.iter().map(|s| s.info.pid).collect();
        let mut exe_pids = render_pids.clone();
        // Capture pids keep their exe cache entry too, or it would be re-resolved every scan.
        if self.cfg.call_to_chat {
            exe_pids.extend(note_captures(&caps, &mut self.captures, now));
        } else {
            self.captures.clear();
            exe_pids.extend(caps.iter().map(|c| c.pid));
        }
        let mut wrote_routes = false;
        if self.cfg.enabled {
            wrote_routes = self.apply_routes(&scan, now);
            self.apply_levels(&scan, now);
        } else {
            self.routes.clear();
            self.applied.clear();
        }
        // Forget dead pids and sessions.
        let keys: HashSet<&str> = scan.sessions.iter().map(|s| s.info.key.as_str()).collect();
        self.routes.retain(|pid, _| render_pids.contains(pid));
        self.exes.retain(|pid, _| exe_pids.contains(pid));
        self.applied.retain(|k, _| keys.contains(k.as_str()));

        let apps = app_sessions(scan.sessions.iter().map(|s| &s.info), &self.cfg, &self.captures, now);
        {
            let mut g = lock(&self.shared);
            g.sessions = apps;
            g.captures = caps
                .into_iter()
                .map(|c| CaptureSession { pid: c.pid, exe: c.exe, device_id: c.device_id, active: c.active })
                .collect();
        }
        self.live = scan.sessions;
        ScanOutcome { wrote_routes, healthy: scan_healthy(true, endpoints, self.policy_calls, self.policy_errors) }
    }

    fn apply_routes(&mut self, scan: &Scan, now: Instant) -> bool {
        let Some(policy) = &self.policy else { return false };
        let mut wrote = false;
        let mut seen = HashSet::new();
        for s in scan.sessions.iter().map(|s| &s.info) {
            if s.exe == UNKNOWN_EXE || !seen.insert(s.pid) {
                continue;
            }
            let channel = self.cfg.effective(&s.exe, &self.captures, now).0;
            let desired = self.cfg.desired_device(channel, &scan.endpoints);
            self.policy_calls += 1;
            let (multimedia, communications) =
                match (policy.get(s.pid, ROLE_MULTIMEDIA), policy.get(s.pid, ROLE_COMMUNICATIONS)) {
                    (Ok(m), Ok(c)) => (m, c),
                    (Err(e), _) | (_, Err(e)) => {
                        self.policy_errors += 1;
                        log::debug!("routing: {e}");
                        continue;
                    }
                };
            let current = mismatched(desired, multimedia.as_deref(), communications.as_deref());
            let target = desired.unwrap_or("Windows default");
            match route_step(self.routes.entry(s.pid).or_default(), desired, current) {
                RouteAction::Keep => continue,
                RouteAction::GiveUp => {
                    log::warn!(
                        "routing: {} (pid {}) keeps refusing {target}; gave up after {MAX_ROUTE_ATTEMPTS} writes",
                        s.exe,
                        s.pid
                    );
                    continue;
                }
                RouteAction::Write => {}
            }
            log::info!("routing: {} (pid {}) -> {target}", s.exe, s.pid);
            wrote = true;
            match policy.set_all_roles(s.pid, desired) {
                Ok(()) => self.footprint.note_route(&s.exe, desired),
                Err(e) => log::debug!("routing: {e}"),
            }
        }
        wrote
    }

    fn apply_levels(&mut self, scan: &Scan, now: Instant) {
        for s in &scan.sessions {
            let i = &s.info;
            if i.exe == UNKNOWN_EXE {
                continue;
            }
            let want = self.cfg.desired_level(self.cfg.effective(&i.exe, &self.captures, now).0);
            let touched = self.touched_exes.contains(&i.exe);
            let writes = level_writes(self.applied.get(&i.key).copied(), want, touched, || read_level(&s.vol));
            match write_level(&s.vol, &writes, want) {
                Ok(()) => {
                    if writes.volume || writes.mute {
                        self.touched_exes.insert(i.exe.clone());
                    }
                    self.footprint.note_levels(&i.exe, &writes);
                    self.applied.insert(i.key.clone(), want);
                }
                Err(e) => log::debug!("routing: volume of {} (pid {}): {e}", i.exe, i.pid),
            }
        }
    }

    /// Routing was turned off: undo what this run wrote. For every exe we routed, the per-app route is
    /// cleared through each of its live pids, and for every exe whose volume / mute we changed, all its
    /// live sessions go back to 100 % / unmuted (as of the last scan: Windows keeps both per exe, so a
    /// restarted app or a new session carries our values without having been written itself). Programs
    /// with no live session cannot be reached (the policy API needs a pid): `reset-routing` covers those.
    /// Never done on process exit. Best effort.
    fn release(&mut self) {
        let fp = std::mem::take(&mut self.footprint);
        if let Some(policy) = &self.policy {
            for pid in fp.routed_pids(self.live.iter().map(|s| (s.info.pid, s.info.exe.as_str()))) {
                if let Err(e) = policy.set_all_roles(pid, None) {
                    log::debug!("routing: release route of pid {pid}: {e}");
                }
            }
        }
        for s in &self.live {
            if let Some(w) = fp.levels.get(&s.info.exe) {
                if let Err(e) = write_level(&s.vol, w, UNTOUCHED) {
                    log::debug!("routing: release volume of {} (pid {}): {e}", s.info.exe, s.info.pid);
                }
            }
        }
        if !fp.routed.is_empty() || !fp.levels.is_empty() {
            log::info!("routing: off, released the routes of {} programs and the levels of {}", fp.routed.len(), fp.levels.len());
        }
        self.routes.clear();
        self.applied.clear();
    }

    fn publish_peaks(&self) {
        let mut by_pid: BTreeMap<u32, f32> = BTreeMap::new();
        for s in &self.live {
            // SAFETY: `meter` is a live IAudioMeterInformation created on this thread.
            let p = unsafe { s.meter.GetPeakValue() }.unwrap_or(0.0).clamp(0.0, 1.0);
            let e = by_pid.entry(s.info.pid).or_insert(0.0);
            *e = e.max(p);
        }
        lock(&self.shared).peaks = by_pid.into_iter().collect();
    }
}

/// Every app capture session on every active capture endpoint. A failure is logged at debug level and
/// gives an empty list.
fn capture_list(en: &IMMDeviceEnumerator, own_pid: u32, exes: &mut HashMap<u32, String>) -> Vec<CaptureInfo> {
    sessions::enumerate_capture(en, own_pid, exes).unwrap_or_else(|e| {
        log::debug!("routing: capture enumeration failed: {e}");
        Vec::new()
    })
}

/// Records `now` for every exe that has an active microphone session in `list` and drops entries older than
/// [`CALL_HANGOVER`]. Returns every capture pid seen (any state), for the exe cache.
fn note_captures(list: &[CaptureInfo], captures: &mut Captures, now: Instant) -> HashSet<u32> {
    let mut pids = HashSet::new();
    for c in list {
        pids.insert(c.pid);
        if c.active && c.exe != UNKNOWN_EXE && captures.insert(c.exe.clone(), now).is_none() {
            log::debug!("routing: microphone in use by {} (pid {}) on {}", c.exe, c.pid, c.device_id);
        }
    }
    captures.retain(|_, t| now.saturating_duration_since(*t) < CALL_HANGOVER);
    pids
}

/// One-shot, read-only enumeration (for `clarim sessions`). Writes nothing; includes sessions
/// whose exe could not be resolved (`exe == UNKNOWN_EXE`). `channel` / `in_call` reflect an active
/// microphone session at this moment (no hangover without history).
pub fn list_sessions_once(cfg: &RoutingConfig) -> Result<Vec<AppSession>> {
    let _com = ComGuard::init()?;
    let en = sessions::device_enumerator()?;
    let own_pid = std::process::id();
    let mut exes = HashMap::new();
    let scan = sessions::enumerate(&en, own_pid, &mut exes)?;
    let now = Instant::now();
    let mut captures = Captures::new();
    if cfg.call_to_chat {
        note_captures(&capture_list(&en, own_pid, &mut exes), &mut captures, now);
    }
    Ok(app_sessions(scan.sessions.iter().map(|s| &s.info), cfg, &captures, now))
}

/// Read-only: the per-app default render endpoint Windows has persisted for `pid` (eMultimedia),
/// as an `IMMDevice::GetId()` string. `None` = no rule, the app follows the Windows default.
pub fn read_persisted_endpoint(pid: u32) -> Result<Option<String>> {
    let _com = ComGuard::init()?;
    // Bound to a local so the factory is released before `_com` uninitialises COM (a tail-expression
    // temporary would be dropped after it).
    let policy = Policy::new()?;
    policy.get(pid, ROLE_MULTIMEDIA)
}

/// ClearAllPersistedApplicationDefaultEndpoints, used by `clarim reset-routing`.
pub fn clear_all_app_routes() -> Result<()> {
    let _com = ComGuard::init()?;
    let policy = Policy::new()?;
    policy.clear_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADSET: &str = "{0.0.0.00000000}.{aaaaaaaa-0000-0000-0000-000000000001}";
    const SPEAKERS: &str = "{0.0.0.00000000}.{bbbbbbbb-0000-0000-0000-000000000002}";

    fn set(m: &mut BTreeMap<ChannelId, ChannelSettings>, ch: ChannelId, f: impl FnOnce(&mut ChannelSettings)) {
        if let Some(c) = m.get_mut(&ch) {
            f(c);
        }
    }

    fn cfg() -> RoutingConfig {
        let mut channels: BTreeMap<ChannelId, ChannelSettings> =
            ChannelId::ALL.iter().map(|&c| (c, ChannelSettings::default())).collect();
        set(&mut channels, ChannelId::Chat, |c| c.device_id = Some(HEADSET.to_owned()));
        set(&mut channels, ChannelId::Media, |c| {
            c.device_id = Some(SPEAKERS.to_owned());
            c.volume = 0.4;
            c.muted = true;
        });
        RoutingConfig {
            channels,
            app_rules: clarim_core::default_app_rules(),
            default_channel: ChannelId::Game,
            enabled: true,
            call_to_chat: true,
        }
    }

    fn info(pid: u32, exe: &str, active: bool, dev: &str) -> SessionInfo {
        SessionInfo {
            pid,
            exe: exe.to_owned(),
            name: exe.to_owned(),
            device_id: dev.to_owned(),
            key: format!("{pid}-{dev}"),
            active,
            peak: 0.0,
        }
    }

    #[test]
    fn channel_resolution() {
        let c = cfg();
        assert_eq!(c.resolve("discord.exe"), (ChannelId::Chat, true));
        assert_eq!(c.resolve("Spotify.EXE"), (ChannelId::Media, true));
        assert_eq!(c.resolve("eldenring.exe"), (ChannelId::Game, false));
        assert_eq!(c.resolve(UNKNOWN_EXE), (ChannelId::Game, false));
        let mut c = c;
        c.default_channel = ChannelId::Aux;
        assert_eq!(c.resolve("eldenring.exe"), (ChannelId::Aux, false));
    }

    #[test]
    fn desired_device_requires_presence() {
        let c = cfg();
        let both = vec![HEADSET.to_owned(), SPEAKERS.to_owned()];
        assert_eq!(c.desired_device(ChannelId::Chat, &both), Some(HEADSET));
        assert_eq!(c.desired_device(ChannelId::Media, &both), Some(SPEAKERS));
        // device configured but unplugged -> None, never the absent id
        assert_eq!(c.desired_device(ChannelId::Chat, &[SPEAKERS.to_owned()]), None);
        assert_eq!(c.desired_device(ChannelId::Chat, &[]), None);
        // no device configured -> follow Windows default
        assert_eq!(c.desired_device(ChannelId::Game, &both), None);
        // id case differences still match, and the active list's spelling is returned
        let upper = vec![HEADSET.to_uppercase()];
        assert_eq!(c.desired_device(ChannelId::Chat, &upper), Some(upper[0].as_str()));
        // empty string configured never matches
        let mut e = cfg();
        set(&mut e.channels, ChannelId::Chat, |c| c.device_id = Some(String::new()));
        assert_eq!(e.desired_device(ChannelId::Chat, &both), None);
        // channel missing from the map
        e.channels.clear();
        assert_eq!(e.desired_device(ChannelId::Chat, &both), None);
    }

    #[test]
    fn desired_level_is_sanitised() {
        let mut c = cfg();
        assert_eq!(c.desired_level(ChannelId::Media), Level { vol: 0.4, muted: true });
        set(&mut c.channels, ChannelId::Game, |s| s.volume = 7.0);
        assert_eq!(c.desired_level(ChannelId::Game).vol, 1.0);
        set(&mut c.channels, ChannelId::Game, |s| s.volume = f32::NAN);
        assert_eq!(c.desired_level(ChannelId::Game).vol, 1.0);
        c.channels.clear();
        assert_eq!(c.desired_level(ChannelId::Game), Level { vol: 1.0, muted: false });
    }

    /// `route_step` as a plain "write now?".
    fn step(st: &mut RouteState, desired: Option<&str>, current: Option<&str>) -> bool {
        route_step(st, desired, current) == RouteAction::Write
    }

    #[test]
    fn route_step_writes_only_on_mismatch() {
        let mut st = RouteState::default();
        // already where we want it, both "default" and explicit
        assert!(!step(&mut st, None, None));
        assert!(!step(&mut st, Some(HEADSET), Some(HEADSET)));
        assert!(!step(&mut st, Some(HEADSET), Some(&HEADSET.to_uppercase())));
        assert_eq!(st.attempts, 0);
        // wrong device -> write
        assert!(step(&mut st, Some(HEADSET), Some(SPEAKERS)));
        assert!(step(&mut st, Some(HEADSET), None));
        // stale rule but channel wants Windows default -> clear
        let mut st = RouteState::default();
        assert!(step(&mut st, None, Some(SPEAKERS)));
    }

    #[test]
    fn route_step_caps_attempts_per_target() {
        let mut st = RouteState::default();
        let writes = (0..10).filter(|_| step(&mut st, Some(HEADSET), Some(SPEAKERS))).count();
        assert_eq!(writes, MAX_ROUTE_ATTEMPTS as usize);
        // success in between does not refund the budget (an app flipping the rule back cannot loop us)
        assert!(!step(&mut st, Some(HEADSET), Some(HEADSET)));
        assert!(!step(&mut st, Some(HEADSET), Some(SPEAKERS)));
        // a new desired device restarts the budget
        assert!(step(&mut st, Some(SPEAKERS), Some(HEADSET)));
        assert_eq!(st.attempts, 1);
        // and so does going back to a previous one
        assert!(step(&mut st, Some(HEADSET), Some(SPEAKERS)));
        assert_eq!(st.attempts, 1);
    }

    #[test]
    fn route_step_reports_the_cap_once_per_target() {
        let mut st = RouteState::default();
        for _ in 0..MAX_ROUTE_ATTEMPTS {
            assert_eq!(route_step(&mut st, Some(HEADSET), Some(SPEAKERS)), RouteAction::Write);
        }
        assert_eq!(route_step(&mut st, Some(HEADSET), Some(SPEAKERS)), RouteAction::GiveUp);
        assert_eq!(route_step(&mut st, Some(HEADSET), Some(SPEAKERS)), RouteAction::Keep);
        assert_eq!(route_step(&mut st, Some(HEADSET), Some(SPEAKERS)), RouteAction::Keep);
        // matching never reports anything, a new target re-arms the report
        assert_eq!(route_step(&mut st, Some(HEADSET), Some(HEADSET)), RouteAction::Keep);
        for _ in 0..MAX_ROUTE_ATTEMPTS {
            assert_eq!(route_step(&mut st, Some(SPEAKERS), Some(HEADSET)), RouteAction::Write);
        }
        assert_eq!(route_step(&mut st, Some(SPEAKERS), Some(HEADSET)), RouteAction::GiveUp);
    }

    #[test]
    fn either_role_mismatch_counts() {
        // Communications differs while Multimedia matches (the user changed it in Windows settings)
        assert_eq!(mismatched(Some(HEADSET), Some(HEADSET), Some(SPEAKERS)), Some(SPEAKERS));
        assert_eq!(mismatched(Some(HEADSET), Some(HEADSET), None), None);
        // Multimedia differs: it wins whatever Communications says
        assert_eq!(mismatched(Some(HEADSET), Some(SPEAKERS), Some(HEADSET)), Some(SPEAKERS));
        assert_eq!(mismatched(None, Some(SPEAKERS), None), Some(SPEAKERS));
        // end to end through route_step: only a full match (case-insensitive) leaves the rule alone
        let up = HEADSET.to_uppercase();
        let write = |d, m, c| step(&mut RouteState::default(), d, mismatched(d, m, c));
        assert!(write(Some(HEADSET), Some(HEADSET), Some(SPEAKERS)));
        assert!(!write(Some(HEADSET), Some(HEADSET), Some(&up)));
        assert!(!write(None, None, None));
    }

    fn lv(vol: f32, muted: bool) -> Level {
        Level { vol, muted }
    }

    #[test]
    fn level_writes_are_change_only() {
        let never = || -> Option<Level> { panic!("current must not be read when last is known") };
        // unchanged -> no write, even if the user moved the Windows mixer in the meantime
        assert_eq!(level_writes(Some(lv(0.5, false)), lv(0.5, false), false, never), Writes::default());
        assert_eq!(level_writes(Some(lv(0.5, false)), lv(0.5004, false), false, never), Writes::default());
        // slider moved -> volume only; mute toggled -> mute only; both -> both
        assert_eq!(level_writes(Some(lv(0.5, false)), lv(0.7, false), false, never), Writes { volume: true, mute: false });
        assert_eq!(level_writes(Some(lv(0.5, false)), lv(0.5, true), false, never), Writes { volume: false, mute: true });
        assert_eq!(level_writes(Some(lv(0.5, false)), lv(0.0, true), false, never), Writes { volume: true, mute: true });
    }

    #[test]
    fn level_writes_first_sight_compares_with_current() {
        let want = lv(0.5, false);
        // already equal -> nothing to write
        assert_eq!(level_writes(None, want, false, || Some(lv(0.5, false))), Writes::default());
        // different volume / mute
        assert_eq!(level_writes(None, want, false, || Some(lv(0.3, false))), Writes { volume: true, mute: false });
        assert_eq!(level_writes(None, want, false, || Some(lv(0.5, true))), Writes { volume: false, mute: true });
        // unreadable -> write both
        assert_eq!(level_writes(None, want, false, || None), Writes { volume: true, mute: true });
        // a wanted mute (even at full volume) is a real setting
        assert_eq!(level_writes(None, lv(1.0, true), false, || None), Writes { volume: true, mute: true });
    }

    #[test]
    fn level_writes_leave_new_sessions_alone_at_default() {
        let never = || -> Option<Level> { panic!("the session must not even be read") };
        // first sight of an app we never wrote, untouched default: write nothing, whatever it has now
        assert_eq!(level_writes(None, UNTOUCHED, false, never), Writes::default());
        // sessions we already applied still get diffs back to the default
        assert_eq!(level_writes(Some(lv(0.4, true)), UNTOUCHED, false, never), Writes { volume: true, mute: true });
        assert_eq!(level_writes(Some(lv(1.0, false)), UNTOUCHED, false, never), Writes::default());
    }

    #[test]
    fn a_new_session_of_an_app_we_wrote_is_compared_even_at_default() {
        // Windows restarts a new session of the app at the level it remembers: ours from earlier in this run
        // (Midia 40 % muted, a call at Chat 60 %, a slider put back to 100 % while the app was closed)
        assert_eq!(level_writes(None, UNTOUCHED, true, || Some(lv(0.4, true))), Writes { volume: true, mute: true });
        assert_eq!(level_writes(None, UNTOUCHED, true, || Some(lv(0.6, false))), Writes { volume: true, mute: false });
        assert_eq!(level_writes(None, UNTOUCHED, true, || Some(lv(1.0, true))), Writes { volume: false, mute: true });
        // at the default already: nothing to do, and an unreadable session gets the default written
        assert_eq!(level_writes(None, UNTOUCHED, true, || Some(UNTOUCHED)), Writes::default());
        assert_eq!(level_writes(None, UNTOUCHED, true, || None), Writes { volume: true, mute: true });
        // a known session is never read again, touched exe or not
        let never = || -> Option<Level> { panic!("current must not be read when last is known") };
        assert_eq!(level_writes(Some(UNTOUCHED), UNTOUCHED, true, never), Writes::default());
    }

    fn footprint_levels(fp: &Footprint, exe: &str) -> Option<Writes> {
        fp.levels.get(exe).copied()
    }

    #[test]
    fn footprint_tracks_what_this_run_wrote_per_exe() {
        let mut fp = Footprint::default();
        // routes: set -> tracked, cleared again -> forgotten
        fp.note_route("spotify.exe", Some(HEADSET));
        fp.note_route("discord.exe", Some(SPEAKERS));
        fp.note_route("discord.exe", None);
        assert_eq!(fp.routed, HashSet::from(["spotify.exe".to_owned()]));
        // levels: only real writes count, and volume / mute accumulate separately
        fp.note_levels("a.exe", &Writes::default());
        assert_eq!(footprint_levels(&fp, "a.exe"), None);
        fp.note_levels("a.exe", &Writes { volume: true, mute: false });
        fp.note_levels("a.exe", &Writes { volume: false, mute: false });
        assert_eq!(footprint_levels(&fp, "a.exe"), Some(Writes { volume: true, mute: false }));
        fp.note_levels("a.exe", &Writes { volume: false, mute: true });
        assert_eq!(footprint_levels(&fp, "a.exe"), Some(Writes { volume: true, mute: true }));
        fp.note_levels("b.exe", &Writes { volume: false, mute: true });
        assert_eq!(footprint_levels(&fp, "b.exe"), Some(Writes { volume: false, mute: true }));
    }

    #[test]
    fn release_reaches_restarted_and_extra_processes_of_a_routed_exe() {
        let mut fp = Footprint::default();
        // we routed Chrome through one pid (say its audio utility process)
        fp.note_route("chrome.exe", Some(SPEAKERS));
        // later the app restarted: its new pids inherit the route, so none of them was a write of ours,
        // and every pid of the exe is cleared, once each, whatever endpoint(s) its sessions live on
        let live = [(30, "chrome.exe"), (10, "chrome.exe"), (20, "discord.exe"), (10, "chrome.exe"), (40, UNKNOWN_EXE)];
        assert_eq!(fp.routed_pids(live.into_iter()), vec![10, 30]);
        // an exe we cleared again has nothing left to release; nothing live of a routed exe -> nothing to do
        fp.note_route("chrome.exe", None);
        assert!(fp.routed_pids(live.into_iter()).is_empty());
        fp.note_route("spotify.exe", Some(HEADSET));
        assert!(fp.routed_pids(live.into_iter()).is_empty());
    }

    #[test]
    fn call_channel_promotes_media_with_hangover() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let media = ChannelId::Media;
        // capture seen right now / 19 s ago -> Chat; 20 s or older -> back on Media
        assert_eq!(call_channel(media, Some(t0), at(0), true), (ChannelId::Chat, true));
        assert_eq!(call_channel(media, Some(t0), at(19), true), (ChannelId::Chat, true));
        assert_eq!(call_channel(media, Some(t0), at(20), true), (media, false));
        assert_eq!(call_channel(media, Some(t0), at(300), true), (media, false));
        // never captured
        assert_eq!(call_channel(media, None, at(5), true), (media, false));
        // feature off: never promoted
        assert_eq!(call_channel(media, Some(t0), at(1), false), (media, false));
        // only Media is promoted: a game with in-game voice stays on Jogo, Chat is already Chat
        for ch in [ChannelId::Game, ChannelId::Chat, ChannelId::Aux] {
            assert_eq!(call_channel(ch, Some(t0), at(1), true), (ch, false));
        }
        // a timestamp from the future (clock hiccup) does not panic and counts as "just now"
        assert_eq!(call_channel(media, Some(at(5)), at(0), true), (ChannelId::Chat, true));
    }

    #[test]
    fn effective_channel_uses_rule_or_default_then_call() {
        let t0 = Instant::now();
        let mut caps = Captures::new();
        caps.insert("chrome.exe".to_owned(), t0);
        caps.insert("eldenring.exe".to_owned(), t0);
        let mut c = cfg();
        // rule says Media -> Chat during the call, has_rule stays true
        assert_eq!(c.effective("chrome.exe", &caps, t0), (ChannelId::Chat, true, true));
        // same exe, no capture -> its rule
        assert_eq!(c.effective("chrome.exe", &Captures::new(), t0), (ChannelId::Media, true, false));
        // a game that captures (in-game voice) stays on Jogo
        assert_eq!(c.effective("eldenring.exe", &caps, t0), (ChannelId::Game, false, false));
        // unruled app whose DEFAULT channel is Media is promoted too
        c.default_channel = ChannelId::Media;
        assert_eq!(c.effective("eldenring.exe", &caps, t0), (ChannelId::Chat, false, true));
        // the toggle off wins over everything
        c.call_to_chat = false;
        assert_eq!(c.effective("chrome.exe", &caps, t0), (ChannelId::Media, true, false));
    }

    #[test]
    fn promoted_app_is_reported_and_levelled_as_chat() {
        let t0 = Instant::now();
        let c = cfg();
        let mut caps = Captures::new();
        caps.insert("chrome.exe".to_owned(), t0);
        let infos = [info(1, "chrome.exe", true, SPEAKERS), info(2, "spotify.exe", true, SPEAKERS)];
        let out = app_sessions(infos.iter(), &c, &caps, t0);
        let got: Vec<_> = out.iter().map(|s| (s.exe.as_str(), s.channel, s.in_call)).collect();
        assert_eq!(got, vec![("chrome.exe", ChannelId::Chat, true), ("spotify.exe", ChannelId::Media, false)]);
        // device and level follow the effective channel: Chat's headset and default level, not Midia's
        let chat = c.effective("chrome.exe", &caps, t0).0;
        assert_eq!(c.desired_device(chat, &[HEADSET.to_owned(), SPEAKERS.to_owned()]), Some(HEADSET));
        assert_eq!(c.desired_level(chat), UNTOUCHED);
        assert_eq!(c.desired_level(ChannelId::Media), lv(0.4, true));
    }

    #[test]
    fn a_scan_is_healthy_only_when_windows_audio_answered() {
        assert!(scan_healthy(true, 3, 4, 0), "all lookups worked");
        assert!(scan_healthy(true, 3, 4, 3), "a few processes refusing is normal");
        assert!(scan_healthy(true, 3, 0, 0), "routing off or no apps: nothing to look up");
        assert!(!scan_healthy(false, 0, 0, 0), "the enumeration failed");
        assert!(!scan_healthy(true, 0, 0, 0), "no playback endpoint at all: Windows Audio is down");
        assert!(!scan_healthy(true, 3, 4, 4), "every routing lookup failed: the old policy factory is dead");
        assert!(!scan_healthy(true, 3, 1, 1));
    }

    #[test]
    fn health_rebuilds_on_the_third_bad_scan_then_every_fifth_and_announces_the_recovery() {
        let mut h = HealthTracker::default();
        assert_eq!(h.note(true), Health::Fine);
        assert_eq!(h.note(false), Health::Waiting);
        assert_eq!(h.note(false), Health::Waiting);
        assert_eq!(h.note(false), Health::Rebuild, "3rd in a row");
        let rest: Vec<Health> = (0..10).map(|_| h.note(false)).collect();
        let rebuilds: Vec<usize> = rest.iter().enumerate().filter(|(_, e)| **e == Health::Rebuild).map(|(i, _)| i + 4).collect();
        assert_eq!(rebuilds, [8, 13], "scans 8 and 13 of the bad spell");
        assert_eq!(h.note(true), Health::Recovered);
        assert_eq!(h.note(true), Health::Fine);
        // a blip of one or two bad scans is not worth a message
        assert_eq!(h.note(false), Health::Waiting);
        assert_eq!(h.note(true), Health::Fine);
    }

    #[test]
    fn idle_engine_is_detected_after_three_seconds() {
        assert!(polled_recently(10_000, 10_000));
        assert!(polled_recently(10_000, 13_000));
        assert!(!polled_recently(10_000, 13_001));
        // a clock that stepped back never panics
        assert!(polled_recently(10_000, 5_000));
        let p = Poll::new();
        assert!(p.recent(), "a fresh engine counts as watched for the first seconds");
        p.touch();
        assert!(p.recent());
    }

    fn capture(pid: u32, exe: &str, active: bool) -> CaptureInfo {
        CaptureInfo { pid, exe: exe.to_owned(), device_id: "mic".to_owned(), active }
    }

    #[test]
    fn note_captures_records_active_known_exes_and_lets_them_expire() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let list = [capture(1, "discord.exe", true), capture(2, "chrome.exe", false), capture(3, UNKNOWN_EXE, true)];
        let mut caps = Captures::new();
        let pids = note_captures(&list, &mut caps, t0);
        assert_eq!(pids, HashSet::from([1, 2, 3]), "every capture pid is kept for the exe cache, any state");
        let exes: Vec<&str> = caps.keys().map(String::as_str).collect();
        assert_eq!(exes, ["discord.exe"], "only an active, resolved exe counts as a call");
        // nothing new later: it stays through the hangover and is gone at 20 s
        note_captures(&[], &mut caps, at(19));
        assert_eq!(caps.len(), 1, "19 s after the last capture it is still in the hangover");
        note_captures(&[], &mut caps, at(20));
        assert!(caps.is_empty(), "20 s after the last capture it expired: {caps:?}");
    }

    #[test]
    fn captures_accessor_returns_the_published_list_without_marking_the_dashboard_as_polling() {
        let published =
            vec![CaptureSession { pid: 7, exe: "discord.exe".into(), device_id: "cable".into(), active: true }];
        let shared = Arc::new(Mutex::new(Shared { captures: published.clone(), ..Shared::default() }));
        let poll = Arc::new(Poll::new());
        const STAMP: u64 = 12_345;
        poll.last_polled.store(STAMP, Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel();
        let eng = RoutingEngine { tx, shared, poll: Arc::clone(&poll), worker: None };
        assert_eq!(eng.captures(), published);
        let after = poll.last_polled.load(Ordering::Relaxed);
        assert_eq!(after, STAMP, "captures() touched the poll stamp: {STAMP} -> {after}");
        // control: peaks() does touch, so the check above can fail
        eng.peaks();
        let touched = poll.last_polled.load(Ordering::Relaxed);
        assert_ne!(touched, STAMP, "peaks() must move the stamp, or this test proves nothing");
    }

    #[test]
    fn sessions_dedupe_by_pid_and_resolve_channels() {
        let c = cfg();
        let infos = [
            info(30, "spotify.exe", false, SPEAKERS),
            info(10, "discord.exe", true, HEADSET),
            info(30, "spotify.exe", true, HEADSET), // same pid on another endpoint, the active one wins
            info(20, "game.exe", true, SPEAKERS),
            info(30, "spotify.exe", false, SPEAKERS),
        ];
        let out = app_sessions(infos.iter(), &c, &Captures::new(), Instant::now());
        let got: Vec<_> = out.iter().map(|s| (s.pid, s.channel, s.has_rule, s.active)).collect();
        assert_eq!(
            got,
            vec![
                (10, ChannelId::Chat, true, true),
                (20, ChannelId::Game, false, true),
                (30, ChannelId::Media, true, true),
            ]
        );
        assert_eq!(out[2].device_id, HEADSET);
    }

    #[test]
    fn disabled_engine_observes_and_stops_cleanly() {
        // The agent keeps the engine behind an `Arc` inside a `Backend: Send + Sync`.
        fn send_sync<T: Send + Sync>() {}
        send_sync::<RoutingEngine>();
        let mut c = cfg();
        c.enabled = false;
        let eng = RoutingEngine::start(c.clone());
        std::thread::sleep(Duration::from_millis(400));
        c.default_channel = ChannelId::Aux;
        eng.update(c);
        std::thread::sleep(Duration::from_millis(400));
        let sessions = eng.sessions();
        let peaks = eng.peaks();
        eprintln!("observed {} sessions, {} peak entries", sessions.len(), peaks.len());
        assert!(sessions.iter().all(|s| s.pid != std::process::id() && s.pid != 0));
        assert!(peaks.iter().all(|&(_, p)| (0.0..=1.0).contains(&p)));
        eng.stop();
    }
}
