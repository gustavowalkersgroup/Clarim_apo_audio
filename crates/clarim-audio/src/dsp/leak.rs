//! Leak detection for calls: the echo of what the speakers play (`EchoDetector`) and the noise that still
//! passes the suppressor (`NoiseFloor`). Both are pure 10 ms-step estimators fed by the DSP thread.
//!
//! `EchoDetector` is the WebRTC ResidualEchoDetector idea in the dB domain over an 80-frame lag window:
//! it correlates the loudness envelope of a speaker reference with the loudness envelope of Clarim's
//! output. An envelope is robust where the waveform is not (the sound card, the room and the denoiser all
//! change the samples, but not when the loud and quiet stretches happen).
//! Design: docs/leak-agc.md section 4.2.

use super::ms_to_db;

/// Lags covered: 0..790 ms in 10 ms steps.
pub const ECHO_LAGS: usize = 80;

/// EMA weight of the reference/capture statistics and covariances (about 5 s of learned frames).
const ALPHA: f32 = 0.002;
/// A reference block above this is "playing".
const ACTIVE_DB: f32 = -60.0;
/// Output levels are clamped up to this: a silent output is a valid quiet reading (the echo is gone), so it
/// must keep decaying an old score instead of freezing it.
const MIN_OUT_DB: f32 = -85.0;
/// Output within this of the user's own voice level is the user talking, not a leak.
const VOICE_MARGIN_DB: f32 = 10.0;
/// Voice level assumed until the AGC knows it.
const ASSUMED_VOICE_DB: f32 = -30.0;
/// Sigma floors: keep a nearly flat envelope from normalising to full scale.
const C_SIGMA_FLOOR: f32 = 3.0;
const R_SIGMA_FLOOR: f32 = 6.0;
/// Speaker echo lags by at least the denoiser's delay (the reader's 2-block offset and the render/capture
/// buffers roughly cancel out): shorter lags are the user's own voice coming back (a monitor, Windows
/// "Escutar este dispositivo"), never a leak. The first lag searched is the denoiser delay in frames, at
/// least 1 and at most this (what DeepFilterNet's 30 ms asks for; also what an unset detector uses).
const MAX_FIRST_LAG: usize = 3;
const LAST_LAG: usize = 77;
/// The score ramps up over this many learned frames.
const LEARNED_FULL: f32 = 500.0;
const LEVEL_ALPHA: f32 = 0.01;
const LEVEL_MIN_FRAMES: u32 = 50;

pub struct EchoDetector {
    /// Circular history of reference levels (dB), with the reference mean/sigma at the time each was pushed.
    hist: [f32; ECHO_LAGS],
    hist_mean: [f32; ECHO_LAGS],
    hist_sigma: [f32; ECHO_LAGS],
    /// Slot of the newest entry.
    head: usize,
    /// How many history entries are above `ACTIVE_DB`.
    active_in_hist: u32,
    r_mean: f32,
    r_var: f32,
    r_init: bool,
    c_mean: f32,
    c_var: f32,
    c_init: bool,
    /// Covariance of the output level with the reference level `k` frames ago.
    cov: [f32; ECHO_LAGS],
    learned: u32,
    best_k: usize,
    score: f32,
    level: f32,
    level_n: u32,
    inactive: u32,
    /// First lag (frames) that can be an echo, see `MAX_FIRST_LAG`.
    first_lag: usize,
}

impl EchoDetector {
    pub fn new() -> Self {
        Self {
            hist: [-90.0; ECHO_LAGS],
            hist_mean: [-90.0; ECHO_LAGS],
            hist_sigma: [0.0; ECHO_LAGS],
            head: 0,
            active_in_hist: 0,
            r_mean: 0.0,
            r_var: 0.0,
            r_init: false,
            c_mean: 0.0,
            c_var: 0.0,
            c_init: false,
            cov: [0.0; ECHO_LAGS],
            learned: 0,
            best_k: 0,
            score: 0.0,
            level: 0.0,
            level_n: 0,
            inactive: 0,
            first_lag: MAX_FIRST_LAG,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The chain's denoiser delay (`MicChain::latency_ms`), which sets the first lag searched: 3 frames for
    /// Strong (30 ms), 1 for Light and Off. `reset` forgets it, so set it before every push.
    pub fn set_denoise_delay_ms(&mut self, ms: f32) {
        self.first_lag = ((ms / 10.0) as usize).clamp(1, MAX_FIRST_LAG);
    }

    /// One 10 ms step. `ref_ms`: mean square of the reference block for this frame. `out_ms`: mean square of
    /// Clarim's output frame. `voice_db`: the user's speech level at the output (`FrameInfo::voice_db`).
    /// A frame without a reference block is simply not pushed.
    pub fn push(&mut self, ref_ms: f32, out_ms: f32, voice_db: Option<f32>) {
        let r = ms_to_db(ref_ms).min(0.0);
        let c = ms_to_db(out_ms).clamp(MIN_OUT_DB, 0.0);
        let active = r > ACTIVE_DB;
        self.inactive = if active { 0 } else { self.inactive.saturating_add(1) };

        // Learn only while a reference is (or within the lag window just was) playing and the output is not
        // the user's own voice. A silent output is learned too (clamped to `MIN_OUT_DB`): it is what ends an
        // echo. The very first learned frame must itself be active: the reference statistics start there.
        let voice = voice_db.filter(|v| v.is_finite()).unwrap_or(ASSUMED_VOICE_DB);
        let learn = (active || (self.active_in_hist > 0 && self.r_init)) && c <= voice - VOICE_MARGIN_DB;
        if learn && !self.r_init {
            (self.r_mean, self.r_var, self.r_init) = (r, 0.0, true);
        }

        // History keeps its time alignment on every frame. Slots pushed before the statistics exist get
        // zero deviation, so they add nothing to the covariances.
        self.head = (self.head + 1) % ECHO_LAGS;
        let h = self.head;
        self.active_in_hist -= u32::from(self.hist[h] > ACTIVE_DB);
        self.active_in_hist += u32::from(active);
        self.hist[h] = r;
        (self.hist_mean[h], self.hist_sigma[h]) = if self.r_init { (self.r_mean, self.r_var.sqrt()) } else { (r, 0.0) };
        if !learn {
            return;
        }

        ema(&mut self.r_mean, &mut self.r_var, r);
        if self.c_init {
            ema(&mut self.c_mean, &mut self.c_var, c);
        } else {
            (self.c_mean, self.c_var, self.c_init) = (c, 0.0, true);
        }
        self.learned = self.learned.saturating_add(1);

        let c_dev = c - self.c_mean;
        let c_sigma = self.c_var.sqrt().max(C_SIGMA_FLOOR);
        let mut ncc = [0.0f32; ECHO_LAGS];
        for (k, n) in ncc.iter_mut().enumerate() {
            let i = (h + ECHO_LAGS - k) % ECHO_LAGS;
            self.cov[k] += ALPHA * (c_dev * (self.hist[i] - self.hist_mean[i]) - self.cov[k]);
            *n = self.cov[k] / (c_sigma * self.hist_sigma[i].max(R_SIGMA_FLOOR));
        }

        // Best interior local maximum; a peak at the edges or a monotone slope is not an echo.
        let mut best: Option<(usize, f32)> = None;
        for k in self.first_lag..=LAST_LAG {
            let v = ncc[k];
            if v >= ncc[k - 1] && v >= ncc[k + 1] && best.is_none_or(|(_, b)| v > b) {
                best = Some((k, v));
            }
        }
        let Some((k, v)) = best else {
            self.score = 0.0;
            return;
        };
        self.best_k = k;
        self.score = v.clamp(0.0, 1.0) * (self.learned as f32 / LEARNED_FULL).min(1.0);

        // Leak level: the output while the reference that explains it is playing.
        if self.hist[(h + ECHO_LAGS - k) % ECHO_LAGS] > ACTIVE_DB {
            self.level = if self.level_n == 0 { c } else { self.level + LEVEL_ALPHA * (c - self.level) };
            self.level_n = self.level_n.saturating_add(1);
        }
    }

    /// 0..1: how well the output's loudness follows the reference's, at a lag of 10..770 ms (30 with Strong).
    /// Frozen while no learning happens (silent reference, the user talking).
    pub fn score(&self) -> f32 {
        self.score
    }

    /// Leak level (dBFS) while the reference at the best lag is active; `None` until known.
    pub fn level_db(&self) -> Option<f32> {
        (self.level_n >= LEVEL_MIN_FRAMES).then_some(self.level)
    }

    pub fn best_lag_ms(&self) -> f32 {
        self.best_k as f32 * 10.0
    }

    /// Frames since the last pushed reference block above -60 dB (saturating).
    pub fn ref_inactive_frames(&self) -> u32 {
        self.inactive
    }
}

impl Default for EchoDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Incremental EMA mean and variance (West).
fn ema(mean: &mut f32, var: &mut f32, x: f32) {
    let d = x - *mean;
    let inc = ALPHA * d;
    *mean += inc;
    *var = (1.0 - ALPHA) * (*var + d * inc);
}

/// 10 ms frames per second of `NoiseFloor` accounting.
const SECOND: u32 = 100;
/// A second needs this many non-speech frames to count.
const MIN_QUIET_FRAMES: usize = 30;
/// Below this speech probability a frame is non-speech.
const QUIET_VAD: f32 = 0.2;
const SECONDS_KEPT: usize = 10;
const MIN_SECONDS: usize = 3;

/// Residual noise at the output during non-speech: the median of the last ten one-second P20 levels.
pub struct NoiseFloor {
    /// Non-speech frame levels (dB) of the current second.
    levels: [f32; SECOND as usize],
    n: usize,
    ticks: u32,
    /// P20 of each valid second, newest overwrites oldest.
    ring: [f32; SECONDS_KEPT],
    len: usize,
    next: usize,
}

impl NoiseFloor {
    pub fn new() -> Self {
        Self { levels: [0.0; SECOND as usize], n: 0, ticks: 0, ring: [0.0; SECONDS_KEPT], len: 0, next: 0 }
    }

    /// Frames with vad < 0.2 count; level = 10*log10(out_ms) floored at -90.
    pub fn push(&mut self, out_ms: f32, vad: f32) {
        if vad < QUIET_VAD {
            self.levels[self.n] = ms_to_db(out_ms);
            self.n += 1;
        }
        self.ticks += 1;
        if self.ticks < SECOND {
            return;
        }
        if self.n >= MIN_QUIET_FRAMES {
            let idx = ((self.n - 1) as f32 * 0.2).round() as usize;
            let (_, p20, _) = self.levels[..self.n].select_nth_unstable_by(idx, f32::total_cmp);
            self.ring[self.next] = *p20;
            self.next = (self.next + 1) % SECONDS_KEPT;
            self.len = (self.len + 1).min(SECONDS_KEPT);
        }
        self.ticks = 0;
        self.n = 0;
    }

    /// Median of the last 10 one-second P20 values, `None` until 3 valid seconds.
    pub fn floor_db(&self) -> Option<f32> {
        if self.len < MIN_SECONDS {
            return None;
        }
        let mut v = self.ring;
        let v = &mut v[..self.len];
        v.sort_unstable_by(f32::total_cmp);
        let mid = self.len / 2;
        Some(if self.len % 2 == 1 { v[mid] } else { (v[mid - 1] + v[mid]) / 2.0 })
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for NoiseFloor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::Noise;

    const VOICE: Option<f32> = Some(-23.0);

    fn pw(db: f32) -> f32 {
        10.0f32.powf(db / 10.0)
    }

    /// Telegraph envelope in dB per frame: alternates -20 / -50, dwell 50..400 ms drawn from `Noise`.
    fn envelope(seed: u32, frames: usize) -> Vec<f32> {
        let mut noise = Noise::new(seed);
        let mut out = Vec::with_capacity(frames + 40);
        let mut high = true;
        while out.len() < frames {
            let dwell = 5 + ((noise.sample() * 0.5 + 0.5) * 36.0) as usize; // 5..=40 frames
            out.extend(std::iter::repeat_n(if high { -20.0 } else { -50.0 }, dwell));
            high = !high;
        }
        out.truncate(frames);
        out
    }

    /// Output power at frame `t`: the reference `delay` frames ago, `atten_db` down, plus independent noise at -70 dB.
    fn echo_ms(refs: &[f32], delay: usize, atten_db: f32, t: usize) -> f32 {
        let echo = t.checked_sub(delay).map_or(0.0, |s| pw(refs[s] - atten_db));
        echo + pw(-70.0)
    }

    /// Feed `frames` frames of `refs` against the output `out(t)`.
    fn run(det: &mut EchoDetector, refs: &[f32], frames: usize, out: impl Fn(usize) -> f32) {
        for (t, &r) in refs.iter().enumerate().take(frames) {
            det.push(pw(r), out(t), VOICE);
        }
    }

    /// 15 s of echo: the envelope back 250 ms later, 30 dB down.
    fn echoing() -> EchoDetector {
        let refs = envelope(0xC0FFEE, 1500);
        let mut det = EchoDetector::new();
        run(&mut det, &refs, 1500, |t| echo_ms(&refs, 25, 30.0, t));
        det
    }

    #[test]
    fn echo_at_250_ms_is_found_with_its_lag_and_level() {
        let det = echoing();
        assert!(det.score() > 0.6, "score {:.3}", det.score());
        assert!((det.best_lag_ms() - 250.0).abs() <= 20.0, "best lag {} ms", det.best_lag_ms());
        let level = det.level_db().expect("level known after 15 s");
        assert!((-80.0..=-50.0).contains(&level), "level {level:.1} dB");
    }

    #[test]
    fn an_independent_envelope_scores_low() {
        let (refs, other) = (envelope(0xC0FFEE, 3000), envelope(0xBADF00D, 3000));
        let mut det = EchoDetector::new();
        run(&mut det, &refs, 3000, |t| pw(other[t] - 30.0) + pw(-70.0));
        assert!(det.score() < 0.3, "score {:.3}", det.score());
    }

    #[test]
    fn a_silent_output_scores_nothing() {
        let refs = envelope(0xC0FFEE, 3000);
        let mut det = EchoDetector::new();
        run(&mut det, &refs, 3000, |_| 0.0);
        assert!(det.score() < 0.05, "score {:.3}", det.score());
    }

    #[test]
    fn a_silent_reference_freezes_the_score_and_counts_inactive_frames() {
        let mut det = echoing();
        let mut frozen = None;
        for i in 0..300u32 {
            det.push(0.0, pw(-70.0), VOICE);
            assert_eq!(det.ref_inactive_frames(), i + 1);
            if i >= 90 {
                // the 80-frame history holds only silence by now: nothing is learned any more
                let bits = *frozen.get_or_insert(det.score().to_bits());
                assert_eq!(det.score().to_bits(), bits, "score moved at silent frame {i}");
            }
        }
        assert!(det.score() > 0.6, "the frozen score is kept for the caller to age out, got {:.3}", det.score());

        let mut fresh = EchoDetector::new();
        for _ in 0..300 {
            fresh.push(0.0, pw(-70.0), VOICE);
        }
        assert_eq!(fresh.score(), 0.0);
        assert_eq!(fresh.ref_inactive_frames(), 300);
    }

    #[test]
    fn the_users_loud_voice_is_never_learned() {
        let mut det = echoing();
        let (score, level) = (det.score().to_bits(), det.level_db().map(f32::to_bits));
        let refs = envelope(0xFACE, 300);
        for &r in &refs {
            det.push(pw(r), pw(-23.0), VOICE); // the output is the user talking at their own level
        }
        assert_eq!(det.score().to_bits(), score, "speech-only frames moved the score");
        assert_eq!(det.level_db().map(f32::to_bits), level, "speech-only frames moved the level");
    }

    #[test]
    fn long_delays_up_to_the_window_are_found_and_beyond_it_are_not() {
        let refs = envelope(0xC0FFEE, 2500);
        let mut det = EchoDetector::new();
        run(&mut det, &refs, 2500, |t| echo_ms(&refs, 60, 30.0, t));
        assert!((det.best_lag_ms() - 600.0).abs() <= 20.0, "best lag {} ms", det.best_lag_ms());
        assert!(det.score() > 0.5, "600 ms echo scored only {:.3}", det.score());

        let refs = envelope(0xC0FFEE, 3000);
        let mut det = EchoDetector::new();
        run(&mut det, &refs, 3000, |t| echo_ms(&refs, 120, 30.0, t));
        assert!(det.score() < 0.3, "a 1200 ms delay scored {:.3}", det.score());
    }

    #[test]
    fn a_self_loop_at_zero_lag_is_not_an_echo() {
        let refs = envelope(0xC0FFEE, 3000);
        let mut det = EchoDetector::new();
        run(&mut det, &refs, 3000, |t| echo_ms(&refs, 0, 30.0, t));
        assert!(det.score() < 0.3, "score {:.3}", det.score());
    }

    #[test]
    fn the_first_lag_searched_follows_the_denoiser_delay() {
        let refs = envelope(0xC0FFEE, 3000);
        let score = |denoise_ms: f32, delay_frames: usize| {
            let mut det = EchoDetector::new();
            det.set_denoise_delay_ms(denoise_ms);
            run(&mut det, &refs, 3000, |t| echo_ms(&refs, delay_frames, 30.0, t));
            det.score()
        };
        for frames in [1, 2] {
            let (strong, light, off) = (score(30.0, frames), score(10.0, frames), score(0.0, frames));
            assert!(strong < 0.3, "Strong keeps its 30 ms guard band, a {frames}-frame loop scored {strong:.3}");
            assert!(light > 0.5, "Light (10 ms) must hear a {frames}-frame echo, scored {light:.3}");
            assert!(off > 0.5, "Off must hear a {frames}-frame echo, scored {off:.3}");
        }
        assert!(score(30.0, 3) > 0.5, "a 30 ms echo is found with Strong, scored {:.3}", score(30.0, 3));
        assert!(score(0.0, 0) < 0.3, "zero lag is never an echo, scored {:.3}", score(0.0, 0));
        let mut unset = EchoDetector::new();
        run(&mut unset, &refs, 3000, |t| echo_ms(&refs, 1, 30.0, t));
        assert!(unset.score() < 0.3, "an unset detector keeps the Strong guard band, scored {:.3}", unset.score());
    }

    #[test]
    fn an_echo_that_goes_away_with_the_output_gone_silent_clears_while_the_reference_keeps_playing() {
        let mut det = echoing();
        assert!(det.score() > 0.6, "the echo was learned first, score {:.3}", det.score());
        // the user lowers the speakers or mutes the headset: the output drops below -85 dBFS, the game plays on
        let refs = envelope(0xFACE, 3000);
        for &r in &refs {
            det.push(pw(r), 0.0, VOICE);
        }
        assert!(det.ref_inactive_frames() < 300, "the reference never went quiet for the caller's stale rule");
        assert!(det.score() < 0.3, "the score stayed at {:.3} after 30 s of silent output", det.score());
        let level = det.level_db().expect("level still known");
        assert!(level < -56.0, "the level stayed at {level:.1} dB after 30 s of silent output");
    }

    #[test]
    fn extreme_inputs_stay_finite() {
        let mut det = EchoDetector::new();
        let values = [0.0, 1e-30, 1.0, f32::NAN, f32::INFINITY];
        for i in 0..2000usize {
            let (r, c) = (values[i % values.len()], values[(i / 3) % values.len()]);
            det.push(r, c, if i % 7 == 0 { Some(f32::NAN) } else { VOICE });
            assert!(det.score().is_finite() && (0.0..=1.0).contains(&det.score()), "score {}", det.score());
            assert!(det.level_db().is_none_or(f32::is_finite) && det.best_lag_ms().is_finite());
        }
    }

    #[test]
    fn steady_non_speech_gives_that_level_as_the_floor() {
        let mut nf = NoiseFloor::new();
        for _ in 0..250 {
            nf.push(pw(-50.0), 0.0);
        }
        assert_eq!(nf.floor_db(), None, "2.5 s is under the 3 valid seconds");
        for _ in 0..250 {
            nf.push(pw(-50.0), 0.0);
        }
        let floor = nf.floor_db().expect("5 s of non-speech");
        assert!((floor + 50.0).abs() <= 0.5, "floor {floor:.2} dB");
    }

    #[test]
    fn clicks_do_not_move_the_floor() {
        let mut nf = NoiseFloor::new();
        for i in 0..1000 {
            nf.push(if i % 10 == 3 { pw(-10.0) } else { pw(-50.0) }, 0.0);
        }
        let floor = nf.floor_db().expect("10 s of non-speech");
        assert!((floor + 50.0).abs() <= 0.5, "floor {floor:.2} dB with 10% clicks at -10 dB");
    }

    #[test]
    fn speech_only_input_has_no_floor() {
        let mut nf = NoiseFloor::new();
        for _ in 0..2000 {
            nf.push(pw(-30.0), 0.9);
        }
        assert_eq!(nf.floor_db(), None);
    }
}
