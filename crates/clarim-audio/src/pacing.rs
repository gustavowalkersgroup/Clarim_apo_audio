//! Render-side pacing as pure logic (no devices, so it is unit-tested): clock-drift control, the
//! one-sample slip, stale-backlog rules, underrun inference and the fade used after a gap.
//!
//! Why: the mic and the virtual cable run on independent clocks, so the number of frames queued between
//! the DSP thread and the audio engine drifts by tens of ppm. The render thread averages that queue level
//! over time (`Pacer::hold`) and, when it leaves a dead band around the target, removes or repeats at most
//! ONE sample per write (`Pacer::slip`, `apply_slip`). A slip merges (or splits) two neighbouring samples
//! instead of cutting one, so it is inaudible even at the 100 per second a 2000 ppm error needs.

use crate::dsp::FRAME;

/// Time constant of the queue-level average.
const AVG_TAU_MS: f32 = 250.0;
/// Level error (frames, 0.5 ms) tolerated without any correction.
const DEAD_BAND: f32 = 24.0;
/// Slip credit gained per frame of error per correcting write (a P loop: the slip rate follows the error).
const SLIP_GAIN: f32 = 0.002;
/// Safety margin on top of one engine period in the queue target (3 ms).
pub const MARGIN_FRAMES: usize = 144;
/// One underrun raises the target by this much (2 ms).
const BUMP_FRAMES: f32 = 96.0;
/// The target never grows past one period plus this much (20 ms).
const MAX_EXTRA_FRAMES: f32 = 960.0;
/// Frames of slack when judging whether the engine got a whole period.
const UNDERRUN_TOL: usize = 8;
/// Samples the first audio after a gap fades in over (2 ms).
const FADE_FRAMES: usize = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slip {
    None,
    /// Merge the first two frames of the chunk into one (queue shrinks by one frame).
    Drop,
    /// Insert one interpolated frame before the chunk (queue grows by one frame).
    Repeat,
}

/// Drift controller for one render stream. The controlled variable is the TIME-AVERAGED number of frames
/// queued (endpoint padding + ring): unlike a snapshot taken at one instant it does not jump by a whole
/// period when the capture and render clocks drift past each other.
pub struct Pacer {
    target: f32,
    max_target: f32,
    avg: f32,
    credit: f32,
}

impl Pacer {
    /// `period` = frames the audio engine takes per tick. The target (period + 3 ms) keeps a block that
    /// arrives a little late from starving the engine, without carrying a whole spare period.
    pub fn new(period: usize) -> Self {
        let target = (period + MARGIN_FRAMES) as f32;
        Self { target, max_target: period as f32 + MAX_EXTRA_FRAMES, avg: target, credit: 0.0 }
    }

    pub fn target_frames(&self) -> usize {
        self.target as usize
    }

    /// The averaged queue level in frames.
    pub fn avg_frames(&self) -> f32 {
        self.avg
    }

    /// The queue held `level` frames for the last `dt_ms` milliseconds.
    pub fn hold(&mut self, level: f32, dt_ms: f32) {
        let a = 1.0 - (-dt_ms.max(0.0) / AVG_TAU_MS).exp();
        self.avg += (level - self.avg) * a;
    }

    /// Correction for the next write: at most one frame, only once the average is outside the dead band.
    pub fn slip(&mut self) -> Slip {
        let err = self.avg - self.target;
        let beyond = err - err.clamp(-DEAD_BAND, DEAD_BAND);
        self.credit = (self.credit + beyond * SLIP_GAIN).clamp(-2.0, 2.0);
        if self.credit >= 1.0 {
            self.credit -= 1.0;
            Slip::Drop
        } else if self.credit <= -1.0 {
            self.credit += 1.0;
            Slip::Repeat
        } else {
            Slip::None
        }
    }

    /// A real underrun: ask for 2 ms more cushion (up to 20 ms), the arrival jitter is bigger than assumed.
    pub fn bump(&mut self) {
        self.target = (self.target + BUMP_FRAMES).min(self.max_target);
    }

    /// The queue was just rebuilt to the target (start, or after a dry spell): forget the stale average.
    pub fn reset(&mut self) {
        self.avg = self.target;
        self.credit = 0.0;
    }
}

/// Applies a slip to the `len` frames at the start of `buf` and returns the new length. `Drop` needs
/// `len >= 2`; `Repeat` needs room for one more frame (`buf.len() > len`) and `last`, the frame written
/// before, to interpolate from. Otherwise the chunk is left alone.
pub fn apply_slip(slip: Slip, last: f32, buf: &mut [f32], len: usize) -> usize {
    match slip {
        Slip::Drop if len >= 2 => {
            buf[0] = (buf[0] + buf[1]) * 0.5;
            buf.copy_within(2..len, 1);
            len - 1
        }
        Slip::Repeat if len >= 1 && buf.len() > len => {
            buf.copy_within(0..len, 1);
            buf[0] = (last + buf[1]) * 0.5;
            len + 1
        }
        _ => len,
    }
}

/// How many of the oldest frames to throw away: all but `keep` once more than `max` are queued (a stall
/// made the backlog stale; playing it would keep the delay forever).
pub fn excess(occupied: usize, max: usize, keep: usize) -> usize {
    if occupied > max {
        occupied - keep
    } else {
        0
    }
}

/// Capture ring backlog to skip: more than 3 frames is stale; keep the newest frame, or nothing when the
/// ring is nearly full (the capture side was dropping samples, so what is left is older than a full ring).
pub fn capture_skip(occupied: usize, capacity: usize) -> usize {
    let keep = if occupied + 2 * FRAME >= capacity { 0 } else { FRAME };
    excess(occupied, 3 * FRAME, keep)
}

/// The engine ran dry: `ticks` engine ticks (each wants a whole `period`) happened since our previous
/// wake, which left `last_total` frames queued (endpoint + what it wrote), and the endpoint is empty now.
/// Counting the ticks is what sees a whole missed period: an endpoint that held 0 frames, or exactly a
/// multiple of the period, looks the same as "already dry" from `last_total` alone. `ticks == 0` (a wake
/// from the DSP thread's bell) never counts: the engine has not asked for anything yet.
pub fn ran_dry(padding: usize, last_total: usize, period: usize, ticks: u32) -> bool {
    ticks >= 1 && padding == 0 && last_total + UNDERRUN_TOL < (ticks as usize).saturating_mul(period)
}

/// True only on the call where `now` turns true: a ring that stays full during a stall is ONE overrun
/// event, not one per packet.
pub fn rising(was: &mut bool, now: bool) -> bool {
    let edge = now && !*was;
    *was = now;
    edge
}

/// Fade the first `FADE_FRAMES` of `buf[..len]` in from silence (click-free restart after a gap).
pub fn fade_in(buf: &mut [f32], len: usize) {
    for (i, s) in buf[..len.min(FADE_FRAMES)].iter_mut().enumerate() {
        *s *= (i + 1) as f32 / FADE_FRAMES as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulates a queue that gains `ppm` frames per million, sampled like the render thread does
    /// (a level hold every 5 ms, a slip decision per 10 ms write). Returns (final level, slips).
    fn simulate(ppm: f32, seconds: usize) -> (f32, u32) {
        let mut p = Pacer::new(480);
        let mut level = p.target_frames() as f32;
        let mut slips = 0;
        for ms in 0..seconds * 1000 {
            level += 48.0 * ppm * 1e-6;
            if ms % 5 == 0 {
                p.hold(level, 5.0);
            }
            if ms % 10 == 0 {
                match p.slip() {
                    Slip::Drop => (level, slips) = (level - 1.0, slips + 1),
                    Slip::Repeat => (level, slips) = (level + 1.0, slips + 1),
                    Slip::None => {}
                }
            }
        }
        (level, slips)
    }

    #[test]
    fn matched_clocks_never_slip_and_drift_is_followed_with_few_slips() {
        assert_eq!(simulate(0.0, 30), (624.0, 0));
        // 200 ppm = 9.6 frames/s: 60 s drift 576 frames, all but the dead band is slipped away
        let (level, slips) = simulate(200.0, 60);
        assert!((level - 624.0).abs() < 120.0, "queue level wandered to {level}");
        assert!((450..600).contains(&slips), "{slips} slips");
        // the other direction
        let (level, slips) = simulate(-100.0, 60);
        assert!((level - 624.0).abs() < 120.0, "queue level wandered to {level}");
        assert!((200..300).contains(&slips), "{slips} slips");
    }

    #[test]
    fn average_follows_the_level_weighted_by_time() {
        let mut p = Pacer::new(480);
        p.hold(1000.0, 2500.0);
        assert!((p.avg_frames() - 1000.0).abs() < 1.0);
        let before = p.avg_frames();
        p.hold(0.0, 1.0); // a brief dip hardly moves it
        assert!(before - p.avg_frames() < 5.0);
    }

    #[test]
    fn bump_is_capped_and_reset_forgets_the_average() {
        let mut p = Pacer::new(480);
        for _ in 0..100 {
            p.bump();
        }
        assert_eq!(p.target_frames(), 480 + 960);
        p.hold(0.0, 5000.0);
        p.reset();
        assert_eq!(p.avg_frames() as usize, p.target_frames());
    }

    #[test]
    fn slips_interpolate_instead_of_cutting() {
        let mut buf = [1.0, 3.0, 5.0, 7.0, 0.0];
        assert_eq!(apply_slip(Slip::Drop, 0.0, &mut buf, 4), 3);
        assert_eq!(buf[..3], [2.0, 5.0, 7.0]);
        let mut buf = [2.0, 4.0, 0.0];
        assert_eq!(apply_slip(Slip::Repeat, 0.0, &mut buf, 2), 3);
        assert_eq!(buf, [1.0, 2.0, 4.0]);
        // no room / too short: untouched
        let mut buf = [2.0, 4.0];
        assert_eq!(apply_slip(Slip::Repeat, 0.0, &mut buf, 2), 2);
        assert_eq!(apply_slip(Slip::Drop, 0.0, &mut buf, 1), 1);
        assert_eq!(buf, [2.0, 4.0]);
    }

    #[test]
    fn backlog_rules() {
        let cap = FRAME * 24;
        assert_eq!(capture_skip(FRAME * 3, cap), 0, "up to 3 frames is normal jitter");
        assert_eq!(capture_skip(FRAME * 5 + 7, cap), FRAME * 4 + 7, "keeps the newest frame");
        assert_eq!(capture_skip(cap - FRAME, cap), cap - FRAME, "a nearly full ring is all stale");
        assert_eq!(excess(FRAME, 3 * FRAME, FRAME), 0);
        assert_eq!(excess(FRAME * 10, 3 * FRAME, FRAME), FRAME * 9);
    }

    #[test]
    fn dry_means_the_engine_ticks_wanted_more_than_was_queued() {
        assert!(ran_dry(0, 144, 480, 1), "only 3 ms queued when a 10 ms tick came");
        assert!(!ran_dry(0, 480, 480, 1), "exactly one period: the engine got all it wanted");
        assert!(!ran_dry(0, 475, 480, 1), "slip tolerance");
        assert!(!ran_dry(100, 144, 480, 1), "padding still there: not dry");
        // a whole missed period: nothing was queued at all, or exactly one period for two ticks
        assert!(ran_dry(0, 0, 480, 1), "an empty endpoint for a full tick");
        assert!(ran_dry(0, 480, 480, 2), "the second tick found nothing");
        assert!(ran_dry(0, 960, 480, 3), "a multiple of the period is not enough for one more tick");
        assert!(!ran_dry(0, 960, 480, 2), "two ticks, two periods");
        // no tick since the last wake (the DSP thread's bell): the engine has not asked yet
        assert!(!ran_dry(0, 0, 480, 0), "waiting for the next block is not an underrun");
        assert!(!ran_dry(0, 144, 480, 0));
    }

    #[test]
    fn a_stall_is_one_event() {
        let (mut was, mut events) = (false, 0);
        for full in [false, true, true, true, false, false, true] {
            events += u32::from(rising(&mut was, full));
        }
        assert_eq!(events, 2);
    }

    #[test]
    fn fade_in_ramps_from_silence() {
        let mut buf = [1.0f32; 200];
        fade_in(&mut buf, 200);
        assert!(buf[0] < 0.02 && (buf[FADE_FRAMES - 1] - 1.0).abs() < 1e-6 && buf[FADE_FRAMES] == 1.0);
        let mut short = [1.0f32; 10];
        fade_in(&mut short, 10);
        assert!(short[9] < 0.2);
    }
}
