//! Voice AGC (the "Volume automático" switch): keeps the user's speech level near `AGC_TARGET_DB`.
//!
//! The level is a weighted dB average of the 10 ms frame RMS over frames the speech detector calls speech
//! (12 adjacent frames at least), so noise, music and the other end of a call never move it. The gain
//! rises slowly (3 dB/s) and falls faster (10 dB/s). It never raises the noise between words above
//! `MAX_OUT_NOISE_DB`, and a fast guard pulls it down when speech would reach the limiter.
//! Design: docs/leak-agc.md section 4.1.

use super::{db_to_lin, lin_to_db, ms_to_db};
use crate::calib::SPEECH_VAD;

/// Speech level the AGC aims for (dB mean of 10 ms frame RMS, dBFS).
pub const AGC_TARGET_DB: f32 = -23.0;
pub const AGC_MAX_GAIN_DB: f32 = 20.0;
pub const AGC_MIN_GAIN_DB: f32 = -12.0;

/// 3 dB/s.
const RISE_DB_PER_FRAME: f32 = 0.03;
/// 10 dB/s.
const FALL_DB_PER_FRAME: f32 = 0.10;
/// Limiter guard and disable ramp.
const FAST_FALL_DB_PER_FRAME: f32 = 0.30;
/// Adjacent speech frames before a frame counts as the user's speech.
const SPEECH_RUN: u32 = 12;
/// Weight kept per learned frame (about 4 s of speech).
const LEVEL_LEAK: f32 = 0.9975;
const CONFIDENT_FRAMES: u32 = 40;
/// Below this speech probability a frame is noise for the noise estimate.
const NOISE_VAD: f32 = 0.2;
/// The noise between words stays under this at the output.
const MAX_OUT_NOISE_DB: f32 = -55.0;
const LEARN_FLOOR_DB: f32 = -60.0;
/// The guard keeps speech peaks under this (the limiter sits at -1 dBFS).
const GUARD_DB: f32 = -3.0;
/// Frames this far below a confident estimate are leaked or distant voices, not the user.
const OUTLIER_DB: f32 = 20.0;

pub struct Agc {
    gain_db: f32,
    /// Adjacent frames with `vad >= SPEECH_VAD`.
    run: u32,
    /// Weighted dB sum and weight: level = num / den.
    num: f32,
    den: f32,
    learned: u32,
    /// Noise-only level before Voice Focus and the gate (minimum follower); `None` until the first noise frame.
    noise_in: Option<f32>,
}

impl Agc {
    pub fn new() -> Self {
        Self { gain_db: 0.0, run: 0, num: 0.0, den: 0.0, learned: 0, noise_in: None }
    }

    /// One FRAME in place. `vad`: RNNoise speech probability of this frame (pre-denoise).
    /// `noise_ms`: mean square after the denoiser, before Voice Focus/gate (`FrameInfo::post_denoise_ms`).
    /// `out_gain_db`: the output gain that follows the AGC (for the limiter guard). `enabled`: auto_level.
    pub fn process(&mut self, buf: &mut [f32], vad: f32, noise_ms: f32, out_gain_db: f32, enabled: bool) {
        let prev_db = self.gain_db;
        let (mut sum, mut peak) = (0.0f32, 0.0f32);
        for &s in buf.iter() {
            sum += s * s;
            peak = peak.max(s.abs());
        }
        let rms_db = ms_to_db(sum / buf.len().max(1) as f32);

        self.run = if vad >= SPEECH_VAD { self.run.saturating_add(1) } else { 0 };
        if self.run >= SPEECH_RUN
            && rms_db > LEARN_FLOOR_DB
            && (self.learned < CONFIDENT_FRAMES || rms_db > self.level() - OUTLIER_DB)
        {
            self.num = self.num * LEVEL_LEAK + rms_db;
            self.den = self.den * LEVEL_LEAK + 1.0;
            self.learned = self.learned.saturating_add(1);
        }
        if vad < NOISE_VAD {
            let n_db = ms_to_db(noise_ms);
            self.noise_in = Some(match self.noise_in {
                None => n_db,
                Some(n) => n + (n_db - n) * if n_db < n { 0.1 } else { 0.01 },
            });
        }

        if enabled {
            if self.learned >= CONFIDENT_FRAMES {
                let mut target = (AGC_TARGET_DB - self.level()).clamp(AGC_MIN_GAIN_DB, AGC_MAX_GAIN_DB);
                if let Some(n) = self.noise_in {
                    target = target.min((MAX_OUT_NOISE_DB - n).max(0.0));
                }
                if self.gain_db > target {
                    self.gain_db = (self.gain_db - FALL_DB_PER_FRAME).max(target);
                } else if self.run >= SPEECH_RUN {
                    self.gain_db = (self.gain_db + RISE_DB_PER_FRAME).min(target);
                }
            }
            if lin_to_db(peak) + self.gain_db + out_gain_db > GUARD_DB {
                self.gain_db = (self.gain_db - FAST_FALL_DB_PER_FRAME).max(AGC_MIN_GAIN_DB);
            }
        } else {
            self.gain_db = if self.gain_db > 0.0 {
                (self.gain_db - FAST_FALL_DB_PER_FRAME).max(0.0)
            } else {
                (self.gain_db + FAST_FALL_DB_PER_FRAME).min(0.0)
            };
        }

        ramp(buf, prev_db, self.gain_db);
    }

    /// Weighted dB average of the learned speech frames (only meaningful once `learned > 0`).
    fn level(&self) -> f32 {
        self.num / self.den.max(f32::MIN_POSITIVE)
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Speech level at the AGC input (dBFS), `None` until confident.
    pub fn input_voice_db(&self) -> Option<f32> {
        (self.learned >= CONFIDENT_FRAMES).then(|| self.level())
    }

    /// Shift the learned levels by a known input change (input gain or Windows mic volume), in dB.
    /// The gain target moves at once; the gain itself follows at the rise/fall rate.
    pub fn shift_db(&mut self, db: f32) {
        self.num += db * self.den;
        if let Some(n) = self.noise_in.as_mut() {
            *n += db;
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for Agc {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply a gain that moves linearly (in amplitude) from `from_db` to `to_db` across the frame, so the
/// gain never steps. Exactly 0 dB on both ends is a bit-exact bypass.
fn ramp(buf: &mut [f32], from_db: f32, to_db: f32) {
    if from_db == 0.0 && to_db == 0.0 {
        return;
    }
    let (g0, g1) = (db_to_lin(from_db), db_to_lin(to_db));
    let step = (g1 - g0) / buf.len().max(1) as f32;
    for (i, s) in buf.iter_mut().enumerate() {
        *s *= g0 + step * (i + 1) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::basic::{Gain, Limiter};
    use crate::dsp::{Block, Noise, FRAME};
    use crate::selftest::Voice;

    /// The speech level metric: dB mean of the 10 ms frame RMS.
    fn speech_db(frames: &[Vec<f32>]) -> f32 {
        let sum: f32 = frames.iter().map(|f| ms_to_db(f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32)).sum();
        sum / frames.len() as f32
    }

    /// `n` frames of the synthetic voice scaled to a speech level of `level_db`.
    fn voice(level_db: f32, n: usize) -> Vec<Vec<f32>> {
        let mut v = Voice::new();
        let mut frames: Vec<Vec<f32>> = (0..n).map(|_| (0..FRAME).map(|_| v.sample()).collect()).collect();
        let k = db_to_lin(level_db - speech_db(&frames));
        frames.iter_mut().flatten().for_each(|s| *s *= k);
        frames
    }

    /// Feed `frames` with a constant `vad`/`noise_ms`; returns the output frames and the gain after each.
    fn feed(agc: &mut Agc, frames: Vec<Vec<f32>>, vad: f32, noise_ms: f32, enabled: bool) -> (Vec<Vec<f32>>, Vec<f32>) {
        let mut gains = Vec::with_capacity(frames.len());
        let outs = frames
            .into_iter()
            .map(|mut f| {
                agc.process(&mut f, vad, noise_ms, 0.0, enabled);
                gains.push(agc.gain_db());
                f
            })
            .collect();
        (outs, gains)
    }

    /// An AGC that has settled on a voice at `level_db`.
    fn trained(level_db: f32) -> Agc {
        let mut agc = Agc::new();
        feed(&mut agc, voice(level_db, 2000), 1.0, 0.0, true);
        agc
    }

    /// `n` frames of constant white noise with RMS `rms_db` (every frame has the same level).
    fn steady(rms_db: f32, n: usize, seed: u32) -> Vec<Vec<f32>> {
        let mut noise = Noise::new(seed);
        let a = db_to_lin(rms_db) * 3.0f32.sqrt(); // uniform [-a, a) has RMS a / sqrt(3)
        (0..n).map(|_| (0..FRAME).map(|_| noise.sample() * a).collect()).collect()
    }

    #[test]
    fn quiet_voice_is_raised_to_the_target_without_passing_the_max_gain() {
        let mut agc = Agc::new();
        let (out, gains) = feed(&mut agc, voice(-37.0, 1500), 1.0, 0.0, true);
        let level = speech_db(&out[1300..]);
        assert!((level - AGC_TARGET_DB).abs() <= 1.0, "output speech level {level:.2} dBFS, target {AGC_TARGET_DB}");
        let max = gains.iter().copied().fold(f32::MIN, f32::max);
        assert!(max <= AGC_MAX_GAIN_DB, "gain reached {max:.2} dB, max {AGC_MAX_GAIN_DB}");
    }

    #[test]
    fn loud_voice_is_brought_down_to_the_target() {
        let mut agc = Agc::new();
        let (out, gains) = feed(&mut agc, voice(-12.0, 500), 1.0, 0.0, true);
        let level = speech_db(&out[300..]);
        assert!((level - AGC_TARGET_DB).abs() <= 1.0, "output speech level {level:.2} dBFS, target {AGC_TARGET_DB}");
        let min = gains.iter().copied().fold(f32::MAX, f32::min);
        assert!(min >= AGC_MIN_GAIN_DB, "gain fell to {min:.2} dB, min {AGC_MIN_GAIN_DB}");
    }

    #[test]
    fn noise_only_never_moves_the_gain() {
        let mut agc = Agc::new();
        let (_, gains) = feed(&mut agc, steady(-30.0, 3000, 7), 0.0, 1e-3, true);
        let worst = gains.iter().fold(0.0f32, |w, g| w.max(g.abs()));
        assert_eq!(worst, 0.0, "gain moved to {worst:.3} dB on noise only");
        assert_eq!(agc.input_voice_db(), None, "noise must not teach a voice level");
    }

    #[test]
    fn noise_cap_limits_the_gain_so_the_noise_stays_under_minus_55() {
        let mut agc = Agc::new();
        let mut speech = voice(-50.0, 3000).into_iter();
        let mut noise = steady(-60.0, 3000, 9).into_iter();
        let mut gains = Vec::new();
        for i in 0..3000 {
            if i % 100 < 50 {
                let mut f = speech.next().unwrap();
                agc.process(&mut f, 1.0, 0.0, 0.0, true);
            } else {
                let mut f = noise.next().unwrap();
                agc.process(&mut f, 0.0, 1e-6, 0.0, true); // post-denoise noise at -60 dBFS
            }
            gains.push(agc.gain_db());
        }
        // uncapped, a -50 dBFS voice would get +20 dB; the -60 dBFS noise allows -55 - (-60) = 5 dB
        let last = gains[gains.len() - 1];
        let max = gains[1000..].iter().copied().fold(f32::MIN, f32::max);
        assert!((last - 5.0).abs() <= 0.5, "gain settled at {last:.2} dB, expected 5");
        assert!(max <= 5.5, "gain reached {max:.2} dB, the cap is 5");
    }

    #[test]
    fn speech_frames_25_db_below_the_estimate_are_ignored_and_15_db_below_are_learned() {
        // -20 dBFS keeps both probes above the -60 dBFS learn floor, so only the outlier rule decides
        let mut agc = trained(-20.0);
        let (est, gain) = (agc.input_voice_db().expect("confident"), agc.gain_db());
        feed(&mut agc, steady(est - 25.0, 200, 3), 1.0, 0.0, true);
        let (est_after, gain_after) = (agc.input_voice_db().unwrap(), agc.gain_db());
        assert!((est_after - est).abs() < 1e-3, "estimate moved {est:.3} -> {est_after:.3} on frames 25 dB down");
        assert!((gain_after - gain).abs() < 0.1, "gain moved {gain:.3} -> {gain_after:.3} on frames 25 dB down");

        let mut agc = trained(-20.0);
        feed(&mut agc, steady(est - 15.0, 100, 3), 1.0, 0.0, true);
        let moved = est - agc.input_voice_db().unwrap();
        assert!(moved > 1.0, "frames 15 dB down moved the estimate only {moved:.2} dB, they are speech");
    }

    #[test]
    fn disabled_from_the_start_is_bit_exact_even_on_loud_input() {
        let mut agc = Agc::new();
        let mut noise = Noise::new(5);
        for n in 0..400 {
            let amp = if n % 10 == 0 { 4.0 } else { 0.9 };
            let dry: Vec<f32> = (0..FRAME).map(|_| noise.sample() * amp).collect();
            let mut buf = dry.clone();
            agc.process(&mut buf, 1.0, 1e-3, 0.0, false);
            assert_eq!(buf, dry, "frame {n} changed while disabled");
        }
        assert_eq!(agc.gain_db(), 0.0);
        assert!(agc.input_voice_db().is_some(), "learning keeps running while disabled");
    }

    #[test]
    fn toggling_off_ramps_to_unity_without_steps() {
        let mut agc = Agc::new();
        feed(&mut agc, voice(-50.0, 1200), 1.0, 0.0, true);
        let start = agc.gain_db();
        assert!(start > AGC_MAX_GAIN_DB - 0.1, "gain only reached {start:.2} dB");
        let c = 0.01f32;
        let mut out = Vec::new();
        let mut off_frames = 0;
        while agc.gain_db() != 0.0 && off_frames < 200 {
            let mut f = vec![c; FRAME];
            agc.process(&mut f, 1.0, 0.0, 0.0, false);
            out.extend(f);
            off_frames += 1;
        }
        assert!(off_frames <= 70, "took {off_frames} frames to reach 0 dB from {start:.2} dB");
        let max_step = (db_to_lin(start) - db_to_lin(start - FAST_FALL_DB_PER_FRAME)) / FRAME as f32 * c;
        let worst = out.windows(2).fold(0.0f32, |w, p| w.max((p[1] - p[0]).abs()));
        assert!(worst <= max_step * 1.001, "per-sample step {worst:.3e}, ramp allows {max_step:.3e}");
        let mut f = vec![c; FRAME];
        agc.process(&mut f, 1.0, 0.0, 0.0, false);
        assert!(f.iter().all(|&s| s == c), "back at 0 dB the block must be a bit-exact bypass");
    }

    #[test]
    fn output_stays_finite_and_in_range_with_plus_20_db_output_gain() {
        let mut agc = Agc::new();
        let (mut gain, mut lim) = (Gain::new(20.0), Limiter::new(-1.0));
        let mut noise = Noise::new(11);
        let mut frames = voice(-30.0, 800);
        for (n, f) in frames.iter_mut().enumerate() {
            if n % 10 == 5 {
                f.iter_mut().for_each(|s| *s = noise.sample() * 3.0); // bursts far above full scale
            }
        }
        for mut f in frames {
            agc.process(&mut f, 1.0, 1e-4, 20.0, true);
            gain.process(&mut f);
            lim.process(&mut f);
            assert!(f.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "sample out of range");
        }
    }

    #[test]
    fn shifting_the_input_moves_the_estimate_at_once_and_the_gain_follows() {
        let mut agc = trained(-37.0);
        let (est, g0) = (agc.input_voice_db().unwrap(), agc.gain_db());
        agc.shift_db(-3.0);
        let shifted = agc.input_voice_db().unwrap();
        assert!((shifted - (est - 3.0)).abs() < 1e-3, "estimate {est:.3} -> {shifted:.3} after shift_db(-3)");
        assert_eq!(agc.gain_db(), g0, "the gain itself must not jump");
        // the signal really is 3 dB quieter now: the gain climbs at the rise rate to the new target
        let (_, gains) = feed(&mut agc, voice(-40.0, 150), 1.0, 0.0, true);
        assert!(gains[9] > g0 + 0.25, "gain {:.3} -> {:.3} after 10 frames, it should already be rising", g0, gains[9]);
        let last = gains[149];
        assert!((last - (g0 + 3.0)).abs() < 0.5, "gain {g0:.2} -> {last:.2}, expected +3 dB");
    }
}
