//! Call-aware policy for the leak warnings and the real mic's Windows volume (design: `docs/leak-agc.md`,
//! section 6.1). Everything here is pure: the backend's 1 s housekeeping tick feeds in measurements and a
//! clock (`now` is always passed in, so the tests drive a fake one), and acts on what comes out.
//!
//! - [`CallTracker`]: is a call app capturing from the virtual mic right now?
//! - [`LeakMonitor`]: hysteresis, rate limit and balloon memory for the echo and noise alerts.
//! - [`MicVolumeGuard`]: when to lower the real mic's Windows volume (clipping) and when to give it back.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use clarim_audio::{AGC_MAX_GAIN_DB, AGC_TARGET_DB};
use clarim_core::{CaptureSession, LeakKind};
use clarim_routing::UNKNOWN_EXE;

/// Apps that capture from the virtual mic without being a call (recording/streaming).
pub const NOT_CALL_EXES: &[&str] = &["obs64.exe", "obs32.exe"];

/// A call stays a call this long after the last active capture session (rides over brief Inactive flips).
const CALL_HANGOVER: Duration = Duration::from_secs(8);

/// What `CallTracker::update` found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CallState {
    pub in_call: bool,
    /// Exe names of the call apps (deduped, sorted, no `unknown`); kept during the hangover.
    pub apps: Vec<String>,
}

/// Turns the routing worker's capture sessions into "in a call or not".
#[derive(Debug, Default)]
pub struct CallTracker {
    last_active: Option<Instant>,
    apps: Vec<String>,
}

impl CallTracker {
    /// Call about once a second. Only an *active* session on the virtual mic's capture endpoint
    /// (`cable_capture_id`, compared case-insensitively) counts, unless its exe is in [`NOT_CALL_EXES`].
    /// An unresolved exe counts for `in_call` but is not named in `apps`. Without a cable id there is
    /// never a call, and the hangover does not carry over.
    pub fn update(&mut self, now: Instant, captures: &[CaptureSession], cable_capture_id: Option<&str>) -> CallState {
        let Some(cable) = cable_capture_id else {
            *self = Self::default();
            return CallState::default();
        };
        let mut apps = Vec::new();
        let mut seen = false;
        for c in captures {
            let on_call_endpoint = c.active
                && c.device_id.eq_ignore_ascii_case(cable)
                && !NOT_CALL_EXES.iter().any(|e| c.exe.eq_ignore_ascii_case(e));
            if on_call_endpoint {
                seen = true;
                if c.exe != UNKNOWN_EXE {
                    apps.push(c.exe.clone());
                }
            }
        }
        if seen {
            apps.sort();
            apps.dedup();
            self.last_active = Some(now);
            self.apps = apps;
        }
        let in_call = self.last_active.is_some_and(|t| now.saturating_duration_since(t) < CALL_HANGOVER);
        if !in_call {
            self.apps.clear();
        }
        CallState { in_call, apps: self.apps.clone() }
    }
}

const ECHO_RAISE_SCORE: f32 = 0.5;
const ECHO_RAISE_DB: f32 = -50.0;
const ECHO_LAG_STABLE_MS: f32 = 20.0;
const ECHO_RAISE_TICKS: u32 = 4;
const ECHO_CLEAR_SCORE: f32 = 0.3;
const ECHO_CLEAR_DB: f32 = -56.0;
const ECHO_CLEAR_TICKS: u32 = 10;
const NOISE_RAISE_DB: f32 = -48.0;
const NOISE_RAISE_TICKS: u32 = 8;
const NOISE_CLEAR_DB: f32 = -54.0;
const NOISE_CLEAR_TICKS: u32 = 10;
/// After a delivered balloon, the same kind warns again only after this long of continuous activity.
const REWARN: Duration = Duration::from_secs(10 * 60);

/// One measurement set for `LeakMonitor::tick` (from `MicStatus`).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct LeakMeasures {
    pub echo_score: f32,
    pub echo_db: Option<f32>,
    pub echo_lag_ms: f32,
    pub noise_db: Option<f32>,
    pub references: u32,
}

/// What the backend should show this tick.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LeakOut {
    /// The current alert (Echo wins over Noise), for the dashboard and the tray tooltip.
    pub alert: Option<LeakKind>,
    /// The balloon to try now. Repeats every tick until `LeakMonitor::delivered`.
    pub balloon: Option<LeakKind>,
}

/// Raise/clear hysteresis of one alert kind plus its balloon memory.
#[derive(Debug, Default)]
struct Latch {
    active: bool,
    /// Consecutive ticks meeting the raise condition (while inactive).
    up: u32,
    /// Consecutive ticks meeting the clear condition (while active).
    down: u32,
    /// A balloon of this kind was delivered in this call.
    delivered: bool,
    /// Since when the current uninterrupted activity counts toward the re-warn: the activation, or the last delivery.
    timer: Option<Instant>,
}

impl Latch {
    fn step(&mut self, now: Instant, raise: bool, clear: bool, raise_ticks: u32, clear_ticks: u32) {
        if self.active {
            self.down = if clear { self.down + 1 } else { 0 };
            if self.down >= clear_ticks {
                self.active = false;
                self.down = 0;
                self.timer = None;
            }
        } else {
            self.up = if raise { self.up + 1 } else { 0 };
            if self.up >= raise_ticks {
                self.active = true;
                self.up = 0;
                self.timer = Some(now);
            }
        }
    }

    fn wants_balloon(&self, now: Instant) -> bool {
        let rewarn_due = self.timer.is_some_and(|t| now.saturating_duration_since(t) >= REWARN);
        self.active && (!self.delivered || rewarn_due)
    }
}

/// Decides when a leak is worth telling the user about. Tick it once a second.
#[derive(Debug, Default)]
pub struct LeakMonitor {
    echo: Latch,
    noise: Latch,
    /// Echo lag of the previous tick (any tick): the echo only counts once the lag holds still.
    prev_lag_ms: Option<f32>,
}

impl LeakMonitor {
    /// Outside a call, or with the warnings off, everything is forgotten (counters, active flags, which
    /// balloons were delivered) and the output is empty.
    ///
    /// Echo raises after 4 consecutive ticks of `references > 0`, score >= 0.5, level >= -50 dBFS and a lag
    /// within 20 ms of the previous tick's (so the first tick after a reset never counts: it has no previous
    /// lag). It clears after 10 consecutive ticks of score < 0.3, no level or level < -56 dBFS. Noise raises
    /// after 8 ticks at >= -48 dBFS and clears after 10 ticks below -54 dBFS (or unknown).
    pub fn tick(&mut self, now: Instant, in_call: bool, enabled: bool, m: &LeakMeasures) -> LeakOut {
        if !in_call || !enabled {
            *self = Self::default();
            return LeakOut::default();
        }
        // Non-finite readings are "nothing there".
        let score = if m.echo_score.is_finite() { m.echo_score } else { 0.0 };
        let echo_db = m.echo_db.filter(|d| d.is_finite());
        let noise_db = m.noise_db.filter(|d| d.is_finite());

        let lag_holds = self.prev_lag_ms.is_some_and(|p| (m.echo_lag_ms - p).abs() <= ECHO_LAG_STABLE_MS);
        self.prev_lag_ms = Some(m.echo_lag_ms);
        let echo_raise = m.references > 0
            && score >= ECHO_RAISE_SCORE
            && echo_db.is_some_and(|d| d >= ECHO_RAISE_DB)
            && lag_holds;
        let echo_clear = score < ECHO_CLEAR_SCORE || echo_db.is_none_or(|d| d < ECHO_CLEAR_DB);
        self.echo.step(now, echo_raise, echo_clear, ECHO_RAISE_TICKS, ECHO_CLEAR_TICKS);

        let noise_raise = noise_db.is_some_and(|d| d >= NOISE_RAISE_DB);
        let noise_clear = noise_db.is_none_or(|d| d < NOISE_CLEAR_DB);
        self.noise.step(now, noise_raise, noise_clear, NOISE_RAISE_TICKS, NOISE_CLEAR_TICKS);

        let alert = if self.echo.active {
            Some(LeakKind::Echo)
        } else if self.noise.active {
            Some(LeakKind::Noise)
        } else {
            None
        };
        // Only the alert on show balloons: a second kind waits behind it until it becomes the alert.
        let balloon = alert.filter(|k| self.latch(*k).wants_balloon(now));
        LeakOut { alert, balloon }
    }

    /// The balloon for `kind` reached the user: no more until 10 min of continuous activity (or the next call).
    pub fn delivered(&mut self, kind: LeakKind, now: Instant) {
        let latch = self.latch_mut(kind);
        latch.delivered = true;
        latch.timer = Some(now);
    }

    fn latch(&self, kind: LeakKind) -> &Latch {
        match kind {
            LeakKind::Echo => &self.echo,
            LeakKind::Noise => &self.noise,
        }
    }

    fn latch_mut(&mut self, kind: LeakKind) -> &mut Latch {
        match kind {
            LeakKind::Echo => &mut self.echo,
            LeakKind::Noise => &mut self.noise,
        }
    }
}

/// Never take more than this from the real mic's Windows volume in total.
const MAX_TAKEN_DB: f32 = 12.0;
/// Size of one lowering step.
const STEP_DB: f32 = 3.0;
/// A volume change smaller than this is read-back noise: it counts as "nothing moved", and a leftover
/// of less than this counts as given back.
const MIN_STEP_DB: f32 = 0.5;
/// Clipped speech frames over the last 2 ticks that trigger a lowering.
const LOWER_CLIPS: u32 = 5;
/// Length of the clip window, and of the check that a lowering helped (ticks).
const WINDOW_TICKS: usize = 10;
const ACTION_COOLDOWN: Duration = Duration::from_secs(10);
/// Giving back needs this long without any clip.
const CLIP_FREE: Duration = Duration::from_secs(60);
/// Ticks of "too quiet even at full AGC gain" before giving back (or telling the user).
const QUIET_TICKS: u32 = 10;
const QUIET_BELOW_TARGET_DB: f32 = 6.0;
const QUIET_GAIN_MARGIN_DB: f32 = 0.5;

/// Why the guard stopped acting for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOff {
    /// The mic has no hardware volume (or no usable range): Windows volume cannot help.
    NoHardware,
    /// Lowering the volume did not reduce the clipping, so it is not the cause.
    Ineffective,
    /// A Windows volume call failed.
    Error,
}

/// What the backend should do with the real mic's Windows volume this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VolAction {
    None,
    /// Lower by this many dB.
    Lower(f32),
    /// Raise (give back) this many dB, never above what was taken.
    Raise(f32),
}

/// One lowering whose effect is still being judged.
#[derive(Debug)]
struct Episode {
    /// Clipped frames over the 10 ticks up to and including the one that lowered.
    before: u32,
    /// Clipped frames over the ticks since.
    after: u32,
    ticks: usize,
    /// What `applied` reported as really lowered (0 until then).
    lowered_db: f32,
}

/// Lowers the real mic's Windows volume when the input clips on speech, and gives it back when the voice stays
/// too quiet. It only decides; the backend reads and writes the endpoint volume and reports back through
/// [`applied`](Self::applied). `taken` survives pauses and restarts of the call (it is persisted by the backend).
#[derive(Debug)]
pub struct MicVolumeGuard {
    taken: f32,
    disabled: Option<GuardOff>,
    /// Clip frames of the last (up to 10) active ticks, oldest first.
    clips: VecDeque<u32>,
    /// Consecutive ticks of "voice too quiet at maximum gain".
    quiet_ticks: u32,
    episode: Option<Episode>,
    // Instants survive an inactive stretch: a clip or an action is still recent when the call resumes.
    last_action: Option<Instant>,
    last_clip: Option<Instant>,
}

impl MicVolumeGuard {
    /// `taken_db` comes from the persisted record (0 when there is none).
    pub fn new(taken_db: f32) -> Self {
        let mut guard = Self {
            taken: 0.0,
            disabled: None,
            clips: VecDeque::with_capacity(WINDOW_TICKS),
            quiet_ticks: 0,
            episode: None,
            last_action: None,
            last_clip: None,
        };
        guard.set_taken(taken_db);
        guard
    }

    /// Call about once a second. `active`: in a call, auto level on, `auto_mic_volume` on, mic running, and the
    /// record's device (if any) is the current input. `clip_delta`: clipped speech frames since the last tick.
    /// `input_voice_db` and `agc_gain_db` are the AGC's readings.
    ///
    /// When not `active` it returns `None` and forgets the windows and counters (`taken` stays).
    ///
    /// The Raise that gives back an ineffective lowering is the last action the guard returns: it comes with
    /// `disabled() == Some(Ineffective)` already set, and the caller must still carry it out.
    pub fn tick(
        &mut self,
        now: Instant,
        active: bool,
        clip_delta: u32,
        input_voice_db: Option<f32>,
        agc_gain_db: f32,
    ) -> VolAction {
        if !active {
            self.clips.clear();
            self.quiet_ticks = 0;
            self.episode = None;
            return VolAction::None;
        }
        if self.clips.len() == WINDOW_TICKS {
            self.clips.pop_front();
        }
        self.clips.push_back(clip_delta);
        if clip_delta > 0 {
            self.last_clip = Some(now);
        }
        let quiet = agc_gain_db >= AGC_MAX_GAIN_DB - QUIET_GAIN_MARGIN_DB
            && input_voice_db.is_some_and(|v| v + agc_gain_db < AGC_TARGET_DB - QUIET_BELOW_TARGET_DB);
        self.quiet_ticks = if quiet { self.quiet_ticks.saturating_add(1) } else { 0 };

        if self.disabled.is_some() {
            return VolAction::None;
        }

        // Did the last lowering help? Judged after 10 active ticks; until then nothing else happens.
        if let Some(ep) = &mut self.episode {
            ep.ticks += 1;
            ep.after = ep.after.saturating_add(clip_delta);
            let (ticks, before, after, lowered) = (ep.ticks, ep.before, ep.after, ep.lowered_db);
            if ticks < WINDOW_TICKS {
                return VolAction::None;
            }
            self.episode = None;
            // Not at most half as many clips as before: the volume is not what makes it clip.
            if lowered > 0.0 && after.saturating_mul(2) > before {
                self.disabled = Some(GuardOff::Ineffective);
                return VolAction::Raise(lowered.min(self.taken));
            }
        }

        let recent = self.clips.iter().rev().take(2).fold(0u32, |a, &c| a.saturating_add(c));
        if recent >= LOWER_CLIPS && self.taken < MAX_TAKEN_DB && self.cooled_down(now) {
            let before = self.clips.iter().fold(0u32, |a, &c| a.saturating_add(c));
            self.episode = Some(Episode { before, after: 0, ticks: 0, lowered_db: 0.0 });
            return VolAction::Lower(STEP_DB);
        }

        let clip_free = self.last_clip.is_none_or(|t| now.saturating_duration_since(t) >= CLIP_FREE);
        if self.taken > 0.0 && self.quiet_ticks >= QUIET_TICKS && clip_free && self.cooled_down(now) {
            return VolAction::Raise(STEP_DB.min(self.taken));
        }
        VolAction::None
    }

    /// The backend changed the volume: `delta_db` is the read-back change (negative = lowered). Starts the
    /// cooldown. A change under 0.5 dB counts as nothing moved: `taken` stays and a lowering that moved nothing
    /// is dropped instead of judged.
    pub fn applied(&mut self, delta_db: f32, now: Instant) {
        self.last_action = Some(now);
        if !delta_db.is_finite() || delta_db.abs() < MIN_STEP_DB {
            if self.episode.as_ref().is_some_and(|e| e.lowered_db <= 0.0) {
                self.episode = None;
            }
            return;
        }
        self.set_taken(self.taken - delta_db);
        if delta_db < 0.0 {
            if let Some(ep) = &mut self.episode {
                ep.lowered_db = -delta_db;
            }
        }
    }

    /// Stop acting for this run (`tick` returns `None` from now on). The first reason sticks.
    pub fn disable(&mut self, reason: GuardOff) {
        if self.disabled.is_none() {
            self.disabled = Some(reason);
        }
        self.episode = None;
    }

    pub fn disabled(&self) -> Option<GuardOff> {
        self.disabled
    }

    /// dB currently taken from the real mic's Windows volume (0..=12).
    pub fn taken_db(&self) -> f32 {
        self.taken
    }

    /// The voice stays quiet at full AGC gain and there is nothing left to give back (or a disabled guard
    /// will never give it back): only the user can help (it keeps being tracked while the guard is disabled).
    pub fn too_quiet(&self) -> bool {
        self.quiet_ticks >= QUIET_TICKS && (self.taken <= 0.0 || self.disabled.is_some())
    }

    fn cooled_down(&self, now: Instant) -> bool {
        self.last_action.is_none_or(|t| now.saturating_duration_since(t) >= ACTION_COOLDOWN)
    }

    fn set_taken(&mut self, db: f32) {
        let db = if db.is_finite() { db.clamp(0.0, MAX_TAKEN_DB) } else { 0.0 };
        self.taken = if db < MIN_STEP_DB { 0.0 } else { db };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CABLE: &str = "{0.0.1.00000000}.{AAAA-BBBB}";

    fn cap(exe: &str, device: &str, active: bool) -> CaptureSession {
        CaptureSession { pid: 1, exe: exe.into(), device_id: device.into(), active }
    }

    /// A fake clock: `at(s)` is `s` seconds after an arbitrary start.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }
        fn at(&self, secs: u64) -> Instant {
            self.0 + Duration::from_secs(secs)
        }
    }

    // ---- CallTracker ----

    fn call(t: &mut CallTracker, c: &Clock, secs: u64, caps: &[CaptureSession]) -> CallState {
        t.update(c.at(secs), caps, Some(CABLE))
    }

    #[test]
    fn an_active_session_on_the_cable_capture_is_a_call() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        let st = call(&mut t, &c, 0, &[cap("discord.exe", CABLE, true)]);
        assert_eq!(st, CallState { in_call: true, apps: vec!["discord.exe".into()] });
    }

    #[test]
    fn the_cable_id_matches_regardless_of_case() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        let st = t.update(c.at(0), &[cap("discord.exe", &CABLE.to_lowercase(), true)], Some(&CABLE.to_uppercase()));
        assert!(st.in_call, "ids differing only in case are the same endpoint: {st:?}");
    }

    #[test]
    fn without_a_cable_id_there_is_never_a_call() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        let st = t.update(c.at(0), &[cap("discord.exe", CABLE, true)], None);
        assert_eq!(st, CallState::default());
    }

    #[test]
    fn losing_the_cable_id_ends_the_call_without_a_hangover() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        assert!(call(&mut t, &c, 0, &[cap("discord.exe", CABLE, true)]).in_call);
        let st = t.update(c.at(1), &[cap("discord.exe", CABLE, true)], None);
        assert_eq!(st, CallState::default(), "no cable, no call");
        let st = call(&mut t, &c, 2, &[]);
        assert_eq!(st, CallState::default(), "the old hangover must not come back with the cable");
    }

    #[test]
    fn inactive_sessions_and_other_endpoints_do_not_count() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        let st = call(&mut t, &c, 0, &[cap("discord.exe", CABLE, false), cap("chrome.exe", "{other}", true)]);
        assert_eq!(st, CallState::default());
    }

    #[test]
    fn obs_is_not_a_call_in_any_spelling() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        let caps = [cap("obs64.exe", CABLE, true), cap("obs32.exe", CABLE, true), cap("OBS64.EXE", CABLE, true)];
        let st = call(&mut t, &c, 0, &caps);
        assert_eq!(st, CallState::default());
        let st = call(&mut t, &c, 1, &[cap("obs64.exe", CABLE, true), cap("zoom.exe", CABLE, true)]);
        assert_eq!(st.apps, ["zoom.exe"], "OBS next to a real call app is left out: {st:?}");
    }

    #[test]
    fn an_unknown_exe_counts_for_the_call_but_is_not_listed() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        let st = call(&mut t, &c, 0, &[cap(UNKNOWN_EXE, CABLE, true)]);
        assert_eq!(st, CallState { in_call: true, apps: vec![] });
        let st = call(&mut t, &c, 1, &[cap(UNKNOWN_EXE, CABLE, true), cap("teams.exe", CABLE, true)]);
        assert_eq!(st.apps, ["teams.exe"]);
    }

    #[test]
    fn apps_are_deduped_and_sorted() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        let caps = [cap("zoom.exe", CABLE, true), cap("discord.exe", CABLE, true), cap("zoom.exe", CABLE, true)];
        let st = call(&mut t, &c, 0, &caps);
        assert_eq!(st.apps, ["discord.exe", "zoom.exe"]);
    }

    #[test]
    fn the_call_rides_over_eight_seconds_of_silence_with_its_apps() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        call(&mut t, &c, 0, &[cap("discord.exe", CABLE, true)]);
        let st = call(&mut t, &c, 1, &[cap("discord.exe", CABLE, false)]);
        assert_eq!(st, CallState { in_call: true, apps: vec!["discord.exe".into()] }, "inactive flip at +1 s");
        let st = call(&mut t, &c, 7, &[]);
        assert!(st.in_call && st.apps == ["discord.exe"], "still in the hangover at +7 s: {st:?}");
        let st = call(&mut t, &c, 8, &[]);
        assert_eq!(st, CallState::default(), "the hangover is over at +8 s");
    }

    #[test]
    fn activity_restarts_the_hangover_and_replaces_the_apps() {
        let (c, mut t) = (Clock::new(), CallTracker::default());
        call(&mut t, &c, 0, &[cap("discord.exe", CABLE, true)]);
        call(&mut t, &c, 6, &[cap("zoom.exe", CABLE, true)]);
        let st = call(&mut t, &c, 13, &[]);
        assert!(st.in_call && st.apps == ["zoom.exe"], "7 s after the last activity: {st:?}");
        assert!(!call(&mut t, &c, 14, &[]).in_call, "8 s after the last activity");
    }

    // ---- LeakMonitor ----

    fn echo(score: f32, db: f32, lag: f32) -> LeakMeasures {
        LeakMeasures { echo_score: score, echo_db: Some(db), echo_lag_ms: lag, noise_db: None, references: 1 }
    }

    fn noise(db: f32) -> LeakMeasures {
        LeakMeasures { noise_db: Some(db), ..LeakMeasures::default() }
    }

    /// A monitor ticked in a call with the warnings on, one second per tick (`s` is the next second to tick).
    struct Run {
        mon: LeakMonitor,
        clock: Clock,
        s: u64,
    }

    impl Run {
        fn new() -> Self {
            Run { mon: LeakMonitor::default(), clock: Clock::new(), s: 0 }
        }
        fn tick(&mut self, m: &LeakMeasures) -> LeakOut {
            let out = self.mon.tick(self.clock.at(self.s), true, true, m);
            self.s += 1;
            out
        }
        fn ticks(&mut self, n: u32, m: &LeakMeasures) -> LeakOut {
            let mut out = LeakOut::default();
            for _ in 0..n {
                out = self.tick(m);
            }
            out
        }
        fn deliver(&mut self, kind: LeakKind) {
            self.mon.delivered(kind, self.clock.at(self.s - 1));
        }
        /// Raises an echo alert (needs the extra first tick for the lag baseline) and returns the output of the last tick.
        fn raise_echo(&mut self) -> LeakOut {
            self.ticks(1 + ECHO_RAISE_TICKS, &echo(0.8, -45.0, 250.0))
        }
    }

    #[test]
    fn echo_raises_after_four_stable_ticks_the_first_having_no_lag_to_compare() {
        let mut r = Run::new();
        let m = echo(0.8, -45.0, 250.0);
        for i in 1..=4 {
            assert_eq!(r.tick(&m).alert, None, "tick {i}: the first has no previous lag, so only 3 count");
        }
        assert_eq!(r.tick(&m).alert, Some(LeakKind::Echo), "fifth tick: the fourth counted one");
    }

    #[test]
    fn echo_thresholds_are_inclusive_at_the_raise_edge() {
        let mut r = Run::new();
        assert_eq!(r.ticks(5, &echo(0.5, -50.0, 250.0)).alert, Some(LeakKind::Echo), "score 0.5 and -50 dBFS raise");
        for (name, m) in [
            ("score just under 0.5", echo(0.499, -45.0, 250.0)),
            ("level just under -50", echo(0.8, -50.1, 250.0)),
            ("no level", LeakMeasures { echo_db: None, ..echo(0.8, -45.0, 250.0) }),
            ("no references", LeakMeasures { references: 0, ..echo(0.8, -45.0, 250.0) }),
            ("NaN score", echo(f32::NAN, -45.0, 250.0)),
        ] {
            let mut r = Run::new();
            let out = r.ticks(30, &m);
            assert_eq!(out.alert, None, "{name} must never raise: {out:?}");
        }
    }

    #[test]
    fn echo_needs_the_lag_to_hold_still_within_twenty_ms() {
        // Alternating by exactly 20 ms is still stable.
        let mut r = Run::new();
        let mut out = LeakOut::default();
        for lag in [250.0, 270.0, 250.0, 270.0, 250.0] {
            out = r.tick(&echo(0.8, -45.0, lag));
        }
        assert_eq!(out.alert, Some(LeakKind::Echo), "20 ms steps are within the limit");

        let mut r = Run::new();
        let mut alerts = Vec::new();
        for lag in [250.0, 250.0, 250.0, 270.5, 270.5, 270.5, 270.5] {
            alerts.push(r.tick(&echo(0.8, -45.0, lag)).alert);
        }
        assert!(alerts.iter().all(Option::is_none), "a 20.5 ms jump restarts the count: {alerts:?}");
        assert_eq!(r.tick(&echo(0.8, -45.0, 270.5)).alert, Some(LeakKind::Echo), "four stable ticks after the jump");
    }

    #[test]
    fn a_wandering_lag_never_raises() {
        let mut r = Run::new();
        for i in 0..60 {
            let lag = if i % 2 == 0 { 100.0 } else { 400.0 };
            let out = r.tick(&echo(0.9, -40.0, lag));
            assert_eq!(out.alert, None, "tick {i}: a lag jumping 300 ms each second is not an echo path");
        }
    }

    #[test]
    fn one_bad_tick_restarts_the_raise_count() {
        let mut r = Run::new();
        r.ticks(4, &echo(0.8, -45.0, 250.0)); // 3 counted
        assert_eq!(r.tick(&echo(0.2, -45.0, 250.0)).alert, None);
        // The next good tick has a previous lag, so 4 more good ticks raise.
        assert_eq!(r.ticks(3, &echo(0.8, -45.0, 250.0)).alert, None);
        assert_eq!(r.tick(&echo(0.8, -45.0, 250.0)).alert, Some(LeakKind::Echo));
    }

    #[test]
    fn echo_clears_only_after_ten_consecutive_low_ticks() {
        let mut r = Run::new();
        r.raise_echo();
        assert_eq!(r.ticks(9, &echo(0.29, -45.0, 250.0)).alert, Some(LeakKind::Echo), "9 low ticks");
        assert_eq!(r.tick(&echo(0.29, -45.0, 250.0)).alert, None, "the 10th clears");
    }

    #[test]
    fn echo_sits_through_the_hysteresis_band() {
        let mut r = Run::new();
        r.raise_echo();
        // Score 0.4 is under the raise level but over the clear level: stays raised forever.
        assert_eq!(r.ticks(50, &echo(0.4, -45.0, 250.0)).alert, Some(LeakKind::Echo), "score band");
        // Level -53 is under the raise level but over the clear level.
        assert_eq!(r.ticks(50, &echo(0.8, -53.0, 250.0)).alert, Some(LeakKind::Echo), "level band");
        assert_eq!(r.ticks(50, &echo(0.8, -56.0, 250.0)).alert, Some(LeakKind::Echo), "-56 itself does not clear");
    }

    #[test]
    fn a_tick_back_in_the_band_restarts_the_clear_count() {
        let mut r = Run::new();
        r.raise_echo();
        r.ticks(9, &echo(0.1, -45.0, 250.0));
        r.tick(&echo(0.4, -45.0, 250.0));
        assert_eq!(r.ticks(9, &echo(0.1, -45.0, 250.0)).alert, Some(LeakKind::Echo), "only 9 since the band tick");
        assert_eq!(r.tick(&echo(0.1, -45.0, 250.0)).alert, None);
    }

    #[test]
    fn echo_clears_on_an_unknown_or_very_low_level() {
        for (name, m) in [
            ("no level", LeakMeasures { echo_db: None, ..echo(0.8, -45.0, 250.0) }),
            ("level -56.1", echo(0.8, -56.1, 250.0)),
            ("NaN score", echo(f32::NAN, -45.0, 250.0)),
        ] {
            let mut r = Run::new();
            r.raise_echo();
            assert_eq!(r.ticks(9, &m).alert, Some(LeakKind::Echo), "{name}: 9 ticks");
            assert_eq!(r.tick(&m).alert, None, "{name}: the 10th clears");
        }
    }

    #[test]
    fn noise_raises_after_eight_ticks_at_minus_48() {
        let mut r = Run::new();
        assert_eq!(r.ticks(7, &noise(-48.0)).alert, None, "7 ticks");
        assert_eq!(r.tick(&noise(-48.0)).alert, Some(LeakKind::Noise), "the 8th raises, -48 included");
        let mut r = Run::new();
        assert_eq!(r.ticks(100, &noise(-48.1)).alert, None, "-48.1 never raises");
        assert_eq!(r.ticks(100, &LeakMeasures::default()).alert, None, "unknown noise never raises");
    }

    #[test]
    fn noise_clears_after_ten_ticks_below_minus_54_or_unknown() {
        let mut r = Run::new();
        r.ticks(8, &noise(-40.0));
        assert_eq!(r.ticks(100, &noise(-54.0)).alert, Some(LeakKind::Noise), "-54 itself does not clear");
        assert_eq!(r.ticks(9, &noise(-54.1)).alert, Some(LeakKind::Noise), "9 ticks below");
        assert_eq!(r.tick(&noise(-54.1)).alert, None, "the 10th clears");

        let mut r = Run::new();
        r.ticks(8, &noise(-40.0));
        assert_eq!(r.ticks(9, &LeakMeasures::default()).alert, Some(LeakKind::Noise));
        assert_eq!(r.tick(&LeakMeasures::default()).alert, None, "unknown level counts as clear");
    }

    #[test]
    fn echo_wins_over_noise_and_noise_shows_once_the_echo_is_gone() {
        let mut r = Run::new();
        r.ticks(8, &noise(-40.0));
        assert!(r.mon.noise.active, "noise is raised before the echo shows up");
        let both = LeakMeasures { noise_db: Some(-40.0), ..echo(0.8, -45.0, 250.0) };
        assert_eq!(r.ticks(5, &both).alert, Some(LeakKind::Echo), "echo takes over");
        let only_noise = LeakMeasures { noise_db: Some(-40.0), ..echo(0.0, -90.0, 0.0) };
        assert_eq!(r.ticks(9, &only_noise).alert, Some(LeakKind::Echo), "echo still held for 9 ticks");
        assert_eq!(r.tick(&only_noise).alert, Some(LeakKind::Noise), "the echo cleared; the noise was never off");
    }

    #[test]
    fn the_balloon_repeats_every_tick_until_delivered() {
        let mut r = Run::new();
        let m = echo(0.8, -45.0, 250.0);
        for i in 1..=4 {
            assert_eq!(r.tick(&m).balloon, None, "tick {i}: no alert yet");
        }
        for i in 0..6 {
            let out = r.tick(&m);
            assert_eq!((out.alert, out.balloon), (Some(LeakKind::Echo), Some(LeakKind::Echo)), "tick {i} since the alert");
        }
        r.deliver(LeakKind::Echo);
        let out = r.tick(&m);
        assert_eq!((out.alert, out.balloon), (Some(LeakKind::Echo), None), "delivered: the alert stays, the balloon stops");
    }

    #[test]
    fn a_delivered_balloon_comes_back_after_ten_minutes_of_continuous_activity() {
        let mut r = Run::new();
        let m = echo(0.8, -45.0, 250.0);
        r.raise_echo();
        r.deliver(LeakKind::Echo);
        let delivered_at = r.s - 1;
        while r.s < delivered_at + 600 {
            let out = r.tick(&m);
            assert_eq!(out.balloon, None, "second {} since delivery", r.s - 1 - delivered_at);
        }
        let out = r.tick(&m);
        assert_eq!(out.balloon, Some(LeakKind::Echo), "600 s after the delivery");
        assert_eq!(r.tick(&m).balloon, Some(LeakKind::Echo), "and again while it is not delivered");
        r.deliver(LeakKind::Echo);
        assert_eq!(r.tick(&m).balloon, None, "a new delivery restarts the 10 minutes");
        r.s += 597;
        assert_eq!(r.tick(&m).balloon, None, "599 s after the second delivery");
        assert_eq!(r.tick(&m).balloon, Some(LeakKind::Echo), "600 s after the second delivery");
    }

    #[test]
    fn a_kind_delivered_in_this_call_stays_quiet_when_it_comes_back_and_waits_a_fresh_ten_minutes() {
        let mut r = Run::new();
        r.raise_echo();
        r.deliver(LeakKind::Echo);
        r.ticks(10, &echo(0.0, -90.0, 0.0)); // clears
        let back = r.raise_echo();
        assert_eq!((back.alert, back.balloon), (Some(LeakKind::Echo), None), "same call, already delivered");
        let raised_at = r.s - 1;
        r.s = raised_at + 599;
        assert_eq!(r.tick(&echo(0.8, -45.0, 250.0)).balloon, None, "599 s after the new activation");
        assert_eq!(r.tick(&echo(0.8, -45.0, 250.0)).balloon, Some(LeakKind::Echo), "600 s after it");
    }

    #[test]
    fn only_the_alert_on_show_balloons_while_another_kind_waits_behind_it() {
        let mut r = Run::new();
        r.ticks(8, &noise(-40.0)); // noise active, balloon pending
        let both = LeakMeasures { noise_db: Some(-40.0), ..echo(0.8, -45.0, 250.0) };
        let out = r.ticks(5, &both);
        assert_eq!((out.alert, out.balloon), (Some(LeakKind::Echo), Some(LeakKind::Echo)), "echo is on show");
        r.deliver(LeakKind::Echo);
        assert_eq!(r.tick(&both).balloon, None, "the undelivered noise does not balloon behind the echo");
        let only_noise = LeakMeasures { noise_db: Some(-40.0), ..echo(0.0, -90.0, 0.0) };
        let out = r.ticks(10, &only_noise);
        assert_eq!((out.alert, out.balloon), (Some(LeakKind::Noise), Some(LeakKind::Noise)), "now the noise is on show");
    }

    #[test]
    fn leaving_the_call_or_turning_it_off_forgets_everything() {
        for (in_call, enabled) in [(false, true), (true, false), (false, false)] {
            let mut r = Run::new();
            r.raise_echo();
            r.deliver(LeakKind::Echo);
            let out = r.mon.tick(r.clock.at(r.s), in_call, enabled, &echo(0.8, -45.0, 250.0));
            assert_eq!(out, LeakOut::default(), "in_call {in_call}, enabled {enabled}");
            // Back in the call: the counters start over (5 ticks) and the delivered memory is gone.
            r.s += 1;
            assert_eq!(r.ticks(4, &echo(0.8, -45.0, 250.0)).alert, None, "counters were reset");
            let out = r.tick(&echo(0.8, -45.0, 250.0));
            assert_eq!((out.alert, out.balloon), (Some(LeakKind::Echo), Some(LeakKind::Echo)), "new call, new balloon");
        }
    }

    #[test]
    fn a_reset_also_forgets_the_previous_lag() {
        let mut r = Run::new();
        r.tick(&echo(0.8, -45.0, 250.0));
        r.mon.tick(r.clock.at(r.s), false, true, &echo(0.8, -45.0, 250.0));
        r.s += 1;
        let out = r.ticks(4, &echo(0.8, -45.0, 250.0));
        assert_eq!(out.alert, None, "after a reset the first tick has no lag to compare again");
    }

    // ---- MicVolumeGuard ----

    /// A guard being ticked in a call, one second per tick.
    struct Vol {
        g: MicVolumeGuard,
        clock: Clock,
        s: u64,
    }

    /// A voice that is fine: gain well below the maximum.
    const OK: (Option<f32>, f32) = (Some(-30.0), 0.0);
    /// A voice that is too quiet even at full gain: -52 + 20 = -32 dBFS, over 6 dB under the -23 target.
    const QUIET: (Option<f32>, f32) = (Some(-52.0), 20.0);

    impl Vol {
        fn new(taken: f32) -> Self {
            Vol { g: MicVolumeGuard::new(taken), clock: Clock::new(), s: 0 }
        }
        fn now(&self) -> Instant {
            self.clock.at(self.s)
        }
        fn tick(&mut self, clip: u32, level: (Option<f32>, f32)) -> VolAction {
            self.s += 1;
            self.g.tick(self.now(), true, clip, level.0, level.1)
        }
        fn ticks(&mut self, n: u32, clip: u32, level: (Option<f32>, f32)) -> VolAction {
            let mut last = VolAction::None;
            for _ in 0..n {
                last = self.tick(clip, level);
            }
            last
        }
        /// Every one of `n` ticks must come back `None`.
        fn never(&mut self, n: u32, clip: u32, level: (Option<f32>, f32), what: &str) {
            for i in 1..=n {
                assert_eq!(self.tick(clip, level), VolAction::None, "{what}: tick {i} of {n}");
            }
        }
        fn applied(&mut self, delta: f32) {
            self.g.applied(delta, self.now());
        }
        /// Clips until the guard lowers, applies `-3` dB and returns after 10 clean ticks (the lowering helped).
        fn lower_and_settle(&mut self) {
            while !matches!(self.tick(5, OK), VolAction::Lower(_)) {}
            self.applied(-3.0);
            assert_eq!(self.ticks(10, 0, OK), VolAction::None, "a clean 10 ticks after the lowering");
        }
    }

    #[test]
    fn five_clips_over_two_ticks_lower_the_volume_by_three_db() {
        let mut v = Vol::new(0.0);
        assert_eq!(v.tick(2, OK), VolAction::None, "2 clips");
        assert_eq!(v.tick(2, OK), VolAction::None, "2 + 2 = 4 clips");
        assert_eq!(v.tick(3, OK), VolAction::Lower(3.0), "2 + 3 = 5 clips");
    }

    #[test]
    fn the_clip_window_is_the_last_two_ticks_only() {
        let mut v = Vol::new(0.0);
        for (i, clip) in [3, 0, 3, 0, 3, 0].into_iter().enumerate() {
            assert_eq!(v.tick(clip, OK), VolAction::None, "tick {i}: never more than 3 in two ticks");
        }
        assert_eq!(Vol::new(0.0).tick(5, OK), VolAction::Lower(3.0), "5 in a single tick is enough");
    }

    #[test]
    fn the_cooldown_is_ten_seconds_from_the_applied_change() {
        let mut v = Vol::new(0.0);
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0));
        v.applied(0.0); // nothing moved: only the cooldown starts, no episode to judge
        for i in 1..10 {
            assert_eq!(v.tick(9, OK), VolAction::None, "second {i} after the action");
        }
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0), "10 s after the action");
    }

    #[test]
    fn nothing_is_lowered_past_twelve_db() {
        let mut v = Vol::new(0.0);
        for step in 1..=4 {
            v.lower_and_settle();
            assert_eq!(v.g.taken_db(), 3.0 * step as f32, "after episode {step}");
        }
        v.never(30, 9, OK, "12 dB taken: no more, however hard it clips");
    }

    #[test]
    fn a_persisted_record_counts_toward_the_limit() {
        let mut v = Vol::new(9.0);
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0), "9 < 12");
        Vol::new(12.0).never(5, 9, OK, "12 is the end");
    }

    #[test]
    fn applied_moves_taken_by_the_read_back_delta_within_zero_and_twelve() {
        let mut v = Vol::new(0.0);
        v.applied(-3.0);
        assert_eq!(v.g.taken_db(), 3.0);
        v.applied(-2.5);
        assert_eq!(v.g.taken_db(), 5.5);
        v.applied(1.5);
        assert_eq!(v.g.taken_db(), 4.0, "positive = given back");
        v.applied(-30.0);
        assert_eq!(v.g.taken_db(), 12.0, "capped at 12");
        v.applied(50.0);
        assert_eq!(v.g.taken_db(), 0.0, "never below 0");
    }

    #[test]
    fn a_change_under_half_a_db_leaves_taken_alone() {
        let mut v = Vol::new(6.0);
        v.applied(-0.4);
        v.applied(0.49);
        v.applied(f32::NAN);
        assert_eq!(v.g.taken_db(), 6.0);
    }

    #[test]
    fn a_leftover_under_half_a_db_counts_as_given_back() {
        let mut v = Vol::new(3.0);
        v.applied(2.7);
        assert_eq!(v.g.taken_db(), 0.0, "0.3 dB left is not worth another round trip");
        assert_eq!(MicVolumeGuard::new(0.3).taken_db(), 0.0);
        assert_eq!(MicVolumeGuard::new(0.5).taken_db(), 0.5);
        assert_eq!(MicVolumeGuard::new(f32::NAN).taken_db(), 0.0);
        assert_eq!(MicVolumeGuard::new(-4.0).taken_db(), 0.0);
        assert_eq!(MicVolumeGuard::new(99.0).taken_db(), 12.0);
    }

    #[test]
    fn an_inactive_tick_forgets_the_windows_but_not_what_was_taken() {
        let mut v = Vol::new(6.0);
        assert_eq!(v.tick(3, OK), VolAction::None);
        v.s += 1;
        assert_eq!(v.g.tick(v.now(), false, 50, OK.0, OK.1), VolAction::None, "inactive ticks do nothing");
        assert_eq!(v.tick(3, OK), VolAction::None, "3 + 3 would have been 6 if the window had survived");
        assert_eq!(v.g.taken_db(), 6.0);
    }

    #[test]
    fn an_inactive_tick_restarts_the_quiet_counter_and_clears_too_quiet() {
        let mut v = Vol::new(0.0);
        v.ticks(10, 0, QUIET);
        assert!(v.g.too_quiet());
        v.s += 1;
        v.g.tick(v.now(), false, 0, QUIET.0, QUIET.1);
        assert!(!v.g.too_quiet(), "not active: no verdict");
        v.ticks(9, 0, QUIET);
        assert!(!v.g.too_quiet(), "9 ticks since the reset");
        v.tick(0, QUIET);
        assert!(v.g.too_quiet());
    }

    #[test]
    fn an_inactive_tick_drops_the_episode_being_judged() {
        let mut v = Vol::new(0.0);
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0));
        v.applied(-3.0);
        v.s += 1;
        v.g.tick(v.now(), false, 0, OK.0, OK.1);
        // 10 hard-clipping ticks would have judged it ineffective; with the episode gone the guard just keeps going.
        let last = v.ticks(10, 9, OK);
        assert_eq!(v.g.disabled(), None, "no verdict across a call boundary (last action {last:?})");
    }

    #[test]
    fn a_lowering_that_leaves_more_than_half_the_clips_is_undone_and_ends_the_guard() {
        let mut v = Vol::new(0.0);
        assert_eq!(v.tick(4, OK), VolAction::None);
        assert_eq!(v.tick(4, OK), VolAction::Lower(3.0), "before = 8 clips");
        v.applied(-3.0);
        // 4 clips after is exactly half of 8: effective. 5 is not.
        for i in 1..10 {
            assert_eq!(v.tick(0, OK), VolAction::None, "tick {i} after the lowering");
        }
        assert_eq!(v.tick(5, OK), VolAction::Raise(3.0), "the 10th tick judges: 5 clips after vs 8 before");
        assert_eq!(v.g.disabled(), Some(GuardOff::Ineffective), "set together with the give-back");
        v.applied(3.0);
        assert_eq!(v.g.taken_db(), 0.0);
        v.never(60, 9, OK, "disabled for the run");
    }

    #[test]
    fn half_the_clips_or_fewer_after_the_lowering_is_effective() {
        let mut v = Vol::new(0.0);
        v.tick(4, OK);
        assert_eq!(v.tick(4, OK), VolAction::Lower(3.0));
        v.applied(-3.0);
        v.ticks(9, 0, OK);
        assert_eq!(v.tick(4, OK), VolAction::None, "4 after vs 8 before is exactly half");
        assert_eq!(v.g.disabled(), None);
        assert_eq!(v.g.taken_db(), 3.0, "the lowering stays");
    }

    #[test]
    fn the_give_back_is_what_was_really_lowered_never_more_than_taken() {
        let mut v = Vol::new(6.0);
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0));
        v.applied(-2.0); // the hardware only moved 2 dB: taken 8
        assert_eq!(v.ticks(10, 9, OK), VolAction::Raise(2.0));
        assert_eq!(v.g.taken_db(), 8.0, "applied() is the backend's job");
    }

    #[test]
    fn no_judgment_for_a_lowering_that_never_happened() {
        let mut v = Vol::new(0.0);
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0));
        // The backend never reported it. The 9 ticks of the episode pass without a Raise or a disable.
        v.never(9, 9, OK, "episode running");
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0), "the episode is over with nothing to judge: it tries again");
        assert_eq!(v.g.disabled(), None);
    }

    #[test]
    fn no_second_lowering_while_one_is_being_judged() {
        let mut v = Vol::new(0.0);
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0));
        v.applied(-3.0);
        // The clock jumps, so the 10 s cooldown is long over: only the running check holds the guard back.
        v.s += 100;
        v.never(9, 9, OK, "check running");
    }

    #[test]
    fn ten_quiet_ticks_with_something_taken_give_three_db_back() {
        let mut v = Vol::new(6.0);
        v.never(9, 0, QUIET, "9 quiet ticks");
        assert_eq!(v.tick(0, QUIET), VolAction::Raise(3.0), "the 10th");
        assert_eq!(v.g.disabled(), None);
    }

    #[test]
    fn a_raise_is_never_more_than_what_was_taken() {
        let mut v = Vol::new(2.0);
        assert_eq!(v.ticks(10, 0, QUIET), VolAction::Raise(2.0));
        let mut v = Vol::new(0.5);
        assert_eq!(v.ticks(10, 0, QUIET), VolAction::Raise(0.5));
    }

    #[test]
    fn a_raise_needs_the_agc_at_its_maximum_and_a_voice_well_under_target() {
        let cases: [(&str, (Option<f32>, f32)); 4] = [
            ("gain 19.4 is not the maximum", (Some(-60.0), 19.4)),
            ("voice at the cable exactly 6 dB under target", (Some(-49.0), 20.0)),
            ("voice level not known yet", (None, 20.0)),
            ("voice fine", (Some(-30.0), 20.0)),
        ];
        for (name, level) in cases {
            let mut v = Vol::new(6.0);
            v.never(30, 0, level, name);
            assert!(!v.g.too_quiet(), "{name}");
        }
        let mut v = Vol::new(6.0);
        assert_eq!(v.ticks(10, 0, (Some(-49.1), 19.5)), VolAction::Raise(3.0), "gain 19.5 and 6.1 dB under count");
    }

    #[test]
    fn one_non_quiet_tick_restarts_the_quiet_count() {
        let mut v = Vol::new(6.0);
        v.ticks(9, 0, QUIET);
        assert_eq!(v.tick(0, OK), VolAction::None);
        v.never(9, 0, QUIET, "9 since the interruption");
        assert_eq!(v.tick(0, QUIET), VolAction::Raise(3.0));
    }

    #[test]
    fn a_raise_waits_sixty_seconds_after_the_last_clip() {
        let mut v = Vol::new(6.0);
        assert_eq!(v.tick(1, QUIET), VolAction::None, "one clipped frame at second 1");
        for s in 2..61 {
            assert_eq!(v.tick(0, QUIET), VolAction::None, "second {s}: less than 60 s after the clip");
        }
        assert_eq!(v.tick(0, QUIET), VolAction::Raise(3.0), "60 s after the clip");
    }

    #[test]
    fn a_clip_from_before_the_call_paused_still_counts_for_the_raise() {
        let mut v = Vol::new(6.0);
        v.tick(1, OK); // second 1
        v.s += 1;
        v.g.tick(v.now(), false, 0, OK.0, OK.1); // second 2: inactive
        v.s += 20;
        v.never(10, 0, QUIET, "only about 30 s after the clip");
    }

    #[test]
    fn a_raise_waits_ten_seconds_after_the_last_action() {
        let mut v = Vol::new(9.0);
        assert_eq!(v.ticks(10, 0, QUIET), VolAction::Raise(3.0));
        v.applied(3.0);
        for i in 1..10 {
            assert_eq!(v.tick(0, QUIET), VolAction::None, "second {i} after the action");
        }
        assert_eq!(v.tick(0, QUIET), VolAction::Raise(3.0), "10 s after it");
    }

    #[test]
    fn a_raise_needs_something_taken_and_with_nothing_taken_the_user_is_told() {
        let mut v = Vol::new(0.0);
        v.never(9, 0, QUIET, "nothing taken");
        assert!(!v.g.too_quiet(), "9 ticks");
        assert_eq!(v.tick(0, QUIET), VolAction::None, "nothing to give back");
        assert!(v.g.too_quiet(), "10 ticks with nothing taken");
        v.tick(0, OK);
        assert!(!v.g.too_quiet(), "the voice recovered");
    }

    #[test]
    fn with_something_taken_it_is_not_too_quiet_yet() {
        let mut v = Vol::new(3.0);
        assert_eq!(v.ticks(10, 0, QUIET), VolAction::Raise(3.0));
        assert!(!v.g.too_quiet(), "the guard still has 3 dB to give back");
        v.applied(3.0);
        v.ticks(10, 0, QUIET);
        assert!(v.g.too_quiet(), "all given back and still quiet");
    }

    #[test]
    fn a_disabled_guard_acts_no_more_but_still_notices_a_quiet_voice() {
        for reason in [GuardOff::NoHardware, GuardOff::Ineffective, GuardOff::Error] {
            let mut v = Vol::new(0.0);
            v.g.disable(reason);
            assert_eq!(v.g.disabled(), Some(reason));
            v.never(5, 9, OK, &format!("{reason:?}: no lowering"));
            v.never(10, 0, QUIET, &format!("{reason:?}: nothing to give back"));
            assert!(v.g.too_quiet(), "{reason:?}: the user is still told about a quiet voice");
        }
        let mut v = Vol::new(6.0);
        v.g.disable(GuardOff::Error);
        v.never(20, 0, QUIET, "no give-back either");
        assert!(v.g.too_quiet(), "6 dB are still taken, but a disabled guard never gives them back: the user is told");
    }

    #[test]
    fn the_first_reason_to_disable_sticks() {
        let mut g = MicVolumeGuard::new(0.0);
        g.disable(GuardOff::NoHardware);
        g.disable(GuardOff::Error);
        assert_eq!(g.disabled(), Some(GuardOff::NoHardware));
    }

    #[test]
    fn disabling_drops_the_episode_being_judged() {
        let mut v = Vol::new(0.0);
        assert_eq!(v.tick(9, OK), VolAction::Lower(3.0));
        v.applied(-3.0);
        v.g.disable(GuardOff::Error);
        v.never(10, 9, OK, "no give-back from an Error-disabled guard");
        assert_eq!(v.g.disabled(), Some(GuardOff::Error), "and the reason did not change to Ineffective");
    }
}
