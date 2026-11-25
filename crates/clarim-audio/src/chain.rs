//! The mic DSP chain, shared by the live pipeline and `selftest`, one 10 ms frame at a time:
//! input gain -> HPF 80 Hz -> denoise (off / RNNoise / DeepFilterNet3) -> Voice Focus -> noise gate
//! -> EQ preset -> tone sliders -> voice AGC (auto level) -> output gain -> limiter -1 dBFS (-> final clamp). The
//! output gain sits BEFORE the limiter so that a boost can never push the signal past the -1 dBFS ceiling.
//!
//! Speech probability (for Voice Focus, the dashboard dot and calibration) always comes from one RNNoise
//! instance that sees the gain + HPF signal every frame. In `light` mode that same instance is the
//! denoiser; in `strong` mode DeepFilterNet3 does the audio and RNNoise's output is discarded (only its
//! VAD is used); in `off` mode too. DFN's own lsnr is not used: one cheap 0..1 probability that behaves
//! the same in every mode is simpler than mapping lsnr dB to a probability, and RNNoise costs ~70 us.

use crate::dsp::agc::Agc;
use crate::dsp::basic::{Gain, Limiter, NoiseGate};
use crate::dsp::denoise::Denoise;
use crate::dsp::eq::{Biquad, PresetEq, ToneEq};
use crate::dsp::focus::VoiceFocus;
use crate::dsp::{Block, FRAME};
use clarim_core::{DenoiseMode, EqPreset, MicSettings, EQ_TONE_RANGE_DB};

/// Voice level assumed until the user calibrates (SPEC).
pub const ASSUMED_VOICE_LEVEL_DB: f32 = -26.0;
/// RNNoise delays its output by one frame (window overlap).
const RNNOISE_LATENCY_MS: f32 = 10.0;

/// Everything the DSP thread can change live (no stream restart). `Copy` so it travels through the
/// command channel by value (no allocation, nothing to free on the DSP thread).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DspParams {
    pub denoise: DenoiseMode,
    pub denoise_strength: f32,
    pub voice_focus: bool,
    /// Voice level + the user's offset, as an RMS in dBFS at the Voice Focus stage (after the input gain).
    /// A calibrated level is stored at unity input gain, so the current input gain is added back in.
    pub focus_threshold_db: f32,
    pub gate_on: bool,
    pub gate_threshold_db: f32,
    pub hpf_on: bool,
    pub auto_level: bool,
    pub eq: EqPreset,
    /// Tone sliders `[bass, mid, treble]` in dB (finite, within +-`EQ_TONE_RANGE_DB`), after the preset.
    pub tone_db: [f32; 3],
    pub input_gain_db: f32,
    pub output_gain_db: f32,
}

/// A tone slider value: clamped to the slider range; a non-finite one would poison the IIR state for good.
fn clamp_tone(db: f32) -> f32 {
    if db.is_finite() {
        db.clamp(-EQ_TONE_RANGE_DB, EQ_TONE_RANGE_DB)
    } else {
        0.0
    }
}

impl DspParams {
    pub fn from_settings(s: &MicSettings) -> Self {
        let input_gain_db = s.input_gain_db.clamp(-20.0, 20.0);
        // The calibration measures the level at unity input gain (`pipeline` subtracts the gain), so
        // changing the gain afterwards does not move the threshold. The assumed level is already "as
        // heard at the focus stage", so the gain is not added to it.
        let voice_level_db = s.voice_level_db.map_or(ASSUMED_VOICE_LEVEL_DB, |v| v + input_gain_db);
        Self {
            denoise: s.denoise,
            denoise_strength: s.denoise_strength.clamp(0.0, 1.0),
            voice_focus: s.voice_focus,
            focus_threshold_db: voice_level_db + s.voice_focus_db.clamp(-40.0, 0.0),
            gate_on: s.gate_on,
            gate_threshold_db: s.gate_threshold_db,
            hpf_on: s.hpf_on,
            auto_level: s.auto_level,
            eq: s.eq,
            tone_db: [s.eq_bass_db, s.eq_mid_db, s.eq_treble_db].map(clamp_tone),
            input_gain_db,
            output_gain_db: s.output_gain_db.clamp(-20.0, 20.0),
        }
    }
}

/// DeepFilterNet attenuation limit for a strength 0..1: 6 dB at 0, 100 dB (= unlimited) at 1,
/// exponential in between (12.2 dB at 0.25, 24.5 dB at 0.5, 49 dB at 0.75).
pub fn atten_lim_db(strength: f32) -> f32 {
    6.0 * (100.0f32 / 6.0).powf(strength.clamp(0.0, 1.0))
}

/// Per-frame results the caller needs.
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// Speech probability 0..1 from RNNoise.
    pub vad: f32,
    /// Mean square of the frame after denoising (calibration measures this).
    pub post_denoise_ms: f32,
    /// Voice Focus is passing audio because of speech (false when Voice Focus is off).
    pub focus_open: bool,
    /// Current AGC gain in dB (0 when auto level is off).
    pub agc_gain_db: f32,
    /// Speech level at the AGC input (dBFS, dB mean of 10 ms frame RMS); `None` until the AGC is confident.
    pub input_voice_db: Option<f32>,
    /// Speech level at the output: `input_voice_db` + AGC gain + output gain.
    pub voice_db: Option<f32>,
}

pub struct MicChain {
    p: DspParams,
    input_gain: Gain,
    hpf: Biquad,
    rnn: Denoise,
    #[cfg(feature = "dfn")]
    dfn: Option<crate::dsp::dfn::Dfn>,
    #[cfg(feature = "dfn")]
    dfn_failed: bool,
    /// What really runs (Strong falls back to Light when DFN is unavailable).
    active: DenoiseMode,
    focus: VoiceFocus,
    gate: NoiseGate,
    eq: PresetEq,
    tone: ToneEq,
    agc: Agc,
    limiter: Limiter,
    output_gain: Gain,
}

impl MicChain {
    /// Builds the chain. In `strong` mode this builds the DeepFilterNet3 model (~0.5 s), so call it on
    /// the thread that will process frames (the model is not `Send`).
    pub fn new(p: &DspParams) -> Self {
        let mut chain = Self {
            p: *p,
            input_gain: Gain::new(p.input_gain_db),
            hpf: Biquad::high_pass(80.0, std::f32::consts::FRAC_1_SQRT_2),
            rnn: Denoise::new(p.denoise_strength),
            #[cfg(feature = "dfn")]
            dfn: None,
            #[cfg(feature = "dfn")]
            dfn_failed: false,
            active: DenoiseMode::Off,
            focus: VoiceFocus::new(p.focus_threshold_db),
            gate: NoiseGate::new(p.gate_threshold_db, 1.0, 120.0, 80.0),
            eq: PresetEq::new(p.eq),
            tone: ToneEq::new(p.tone_db),
            agc: Agc::new(),
            limiter: Limiter::new(-1.0),
            output_gain: Gain::new(p.output_gain_db),
        };
        chain.select_denoise();
        chain
    }

    /// Apply new live parameters. Switching to `strong` for the first time in this chain's life builds
    /// the DFN model right here (~0.5 s stall of this thread, once); starting in `strong` avoids it.
    pub fn set_params(&mut self, p: &DspParams) {
        let old = std::mem::replace(&mut self.p, *p);
        if p.input_gain_db != old.input_gain_db {
            self.agc.shift_db(p.input_gain_db - old.input_gain_db); // the AGC input moves by the same dB
        }
        self.input_gain.set_db(p.input_gain_db);
        self.output_gain.set_db(p.output_gain_db);
        self.rnn.set_strength(p.denoise_strength);
        self.gate.set_threshold_db(p.gate_threshold_db);
        self.focus.set_threshold_db(p.focus_threshold_db);
        self.eq.set_preset(p.eq);
        self.tone.set(p.tone_db);
        #[cfg(feature = "dfn")]
        if p.denoise_strength != old.denoise_strength {
            if let Some(dfn) = self.dfn.as_mut() {
                dfn.set_atten_lim(atten_lim_db(p.denoise_strength));
            }
        }
        if p.denoise != old.denoise {
            self.select_denoise();
        }
    }

    /// The mic's level just changed by `db` outside the chain (the Windows mic volume): the AGC follows.
    pub fn shift_input_db(&mut self, db: f32) {
        self.agc.shift_db(db);
    }

    /// The denoiser that is really running.
    pub fn denoise_active(&self) -> DenoiseMode {
        self.active
    }

    /// Algorithmic delay of the chain in ms (the denoiser's; everything else is sample-by-sample).
    pub fn latency_ms(&self) -> f32 {
        match self.active {
            DenoiseMode::Off => 0.0,
            DenoiseMode::Light => RNNOISE_LATENCY_MS,
            DenoiseMode::Strong => self.dfn_latency_ms(),
        }
    }

    fn select_denoise(&mut self) {
        let wanted = self.p.denoise;
        self.active = if wanted == DenoiseMode::Strong && !self.ensure_dfn() { DenoiseMode::Light } else { wanted };
    }

    #[cfg(feature = "dfn")]
    fn ensure_dfn(&mut self) -> bool {
        if self.dfn.is_none() && !self.dfn_failed {
            match crate::dsp::dfn::Dfn::new(atten_lim_db(self.p.denoise_strength)) {
                Ok(dfn) => self.dfn = Some(dfn),
                Err(e) => {
                    log::error!("DeepFilterNet3 init failed, falling back to RNNoise: {e:#}");
                    self.dfn_failed = true;
                }
            }
        }
        self.dfn.is_some()
    }

    #[cfg(not(feature = "dfn"))]
    fn ensure_dfn(&mut self) -> bool {
        false
    }

    #[cfg(feature = "dfn")]
    fn dfn_latency_ms(&self) -> f32 {
        self.dfn.as_ref().map_or(RNNOISE_LATENCY_MS, |d| d.latency_ms())
    }

    #[cfg(not(feature = "dfn"))]
    fn dfn_latency_ms(&self) -> f32 {
        RNNOISE_LATENCY_MS
    }

    /// One DFN frame; on any failure the model is dropped for good and RNNoise takes over this frame.
    #[cfg(feature = "dfn")]
    fn run_dfn(&mut self, buf: &mut [f32]) {
        let ok = match self.dfn.as_mut() {
            Some(dfn) => match dfn.try_process(buf) {
                Ok(()) => true,
                Err(e) => {
                    log::error!("DeepFilterNet3 failed, falling back to RNNoise: {e:#}");
                    false
                }
            },
            None => false,
        };
        if !ok {
            self.dfn = None;
            self.dfn_failed = true;
            self.active = DenoiseMode::Light;
            self.rnn.mix_wet(buf);
        }
    }

    #[cfg(not(feature = "dfn"))]
    fn run_dfn(&mut self, _buf: &mut [f32]) {}

    /// Process one `FRAME`-sized buffer in place. No allocation, no locking (DeepFilterNet3 itself
    /// allocates inside tract; everything of ours does not).
    pub fn process(&mut self, buf: &mut [f32]) -> FrameInfo {
        debug_assert_eq!(buf.len(), FRAME);
        let p = self.p;
        self.input_gain.process(buf);
        if p.hpf_on {
            self.hpf.process(buf);
        }
        let vad = self.rnn.analyze(buf);
        match self.active {
            DenoiseMode::Off => {}
            DenoiseMode::Light => self.rnn.mix_wet(buf),
            DenoiseMode::Strong => self.run_dfn(buf),
        }
        // One trust boundary: a NaN from a neural net would poison every IIR state downstream for good.
        for s in buf.iter_mut() {
            if !s.is_finite() {
                *s = 0.0;
            }
        }
        let post_denoise_ms = buf.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
        let focus_open = self.focus.process(buf, vad, p.voice_focus);
        if p.gate_on {
            self.gate.process(buf);
        }
        if p.eq != EqPreset::Flat {
            self.eq.process(buf);
        }
        self.tone.process(buf); // bypasses itself while all three sliders are 0
        // Always called: it keeps learning the voice level and ramps back to unity when auto level is off.
        self.agc.process(buf, vad, post_denoise_ms, p.output_gain_db, p.auto_level);
        self.output_gain.process(buf);
        self.limiter.process(buf);
        for s in buf.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        let agc_gain_db = self.agc.gain_db();
        let input_voice_db = self.agc.input_voice_db();
        FrameInfo {
            vad,
            post_denoise_ms,
            focus_open,
            agc_gain_db,
            input_voice_db,
            voice_db: input_voice_db.map(|v| v + agc_gain_db + p.output_gain_db),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::SPEECH_VAD;
    use crate::dsp::{Noise, SAMPLE_RATE};
    use crate::selftest::Voice;
    use std::f32::consts::TAU;

    /// Everything off, 0 dB gains.
    fn passthrough() -> DspParams {
        DspParams {
            denoise: DenoiseMode::Off,
            denoise_strength: 1.0,
            voice_focus: false,
            focus_threshold_db: -40.0,
            gate_on: false,
            gate_threshold_db: -50.0,
            hpf_on: false,
            auto_level: false,
            eq: EqPreset::Flat,
            tone_db: [0.0; 3],
            input_gain_db: 0.0,
            output_gain_db: 0.0,
        }
    }

    fn run_sine(chain: &mut MicChain, freq: f32, amp: f32, frames: usize) -> (f32, f32) {
        let (mut ins, mut outs) = (0.0f64, 0.0f64);
        for n in 0..frames {
            let mut buf: Vec<f32> =
                (0..FRAME).map(|k| ((n * FRAME + k) as f32 / SAMPLE_RATE * freq * TAU).sin() * amp).collect();
            let dry = buf.clone();
            chain.process(&mut buf);
            if n >= frames / 2 {
                ins += dry.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
                outs += buf.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
            }
        }
        (ins as f32, outs as f32)
    }

    #[test]
    fn off_with_everything_disabled_is_bit_exact() {
        let mut chain = MicChain::new(&passthrough());
        assert_eq!(chain.denoise_active(), DenoiseMode::Off);
        let mut noise = Noise::new(11);
        for _ in 0..300 {
            let dry: Vec<f32> = (0..FRAME).map(|_| noise.sample() * 0.5).collect();
            let mut buf = dry.clone();
            chain.process(&mut buf);
            assert_eq!(buf, dry);
        }
    }

    #[test]
    fn hpf_attenuates_30_hz_and_passes_1_khz() {
        let mut chain = MicChain::new(&DspParams { hpf_on: true, ..passthrough() });
        let (i, o) = run_sine(&mut chain, 30.0, 0.3, 200);
        let att = 10.0 * (i / o).log10();
        assert!(att >= 12.0, "30 Hz attenuated only {att:.1} dB");
        let mut chain = MicChain::new(&DspParams { hpf_on: true, ..passthrough() });
        let (i, o) = run_sine(&mut chain, 1000.0, 0.3, 200);
        let diff = (10.0 * (o / i).log10()).abs();
        assert!(diff < 1.0, "1 kHz changed by {diff:.2} dB");
    }

    #[test]
    fn params_map_from_settings() {
        let s = MicSettings { voice_level_db: Some(-22.0), voice_focus_db: -14.0, ..MicSettings::default() };
        assert_eq!(DspParams::from_settings(&s).focus_threshold_db, -36.0);
        let s = MicSettings { voice_level_db: None, ..s };
        assert_eq!(DspParams::from_settings(&s).focus_threshold_db, -40.0);
        let s = MicSettings { input_gain_db: 99.0, output_gain_db: -99.0, ..s };
        let p = DspParams::from_settings(&s);
        assert_eq!((p.input_gain_db, p.output_gain_db), (20.0, -20.0));
        let s = MicSettings { eq_bass_db: 40.0, eq_mid_db: -40.0, eq_treble_db: f32::NAN, ..s };
        assert_eq!(DspParams::from_settings(&s).tone_db, [12.0, -12.0, 0.0]);
    }

    #[test]
    fn tone_sliders_apply_live_through_set_params() {
        let mut chain = MicChain::new(&passthrough());
        chain.set_params(&DspParams { tone_db: [6.0, 0.0, 0.0], ..passthrough() });
        let (i, o) = run_sine(&mut chain, 80.0, 0.1, 100);
        let gain = 10.0 * (o / i).log10();
        assert!((gain - 6.0).abs() < 1.0, "+6 dB bass gave {gain:.1} dB at 80 Hz");
        chain.set_params(&passthrough());
        let (i, o) = run_sine(&mut chain, 80.0, 0.1, 100);
        assert!((10.0 * (o / i).log10()).abs() < 0.05, "back at 0 dB the chain is unity again");
    }

    #[test]
    fn focus_threshold_follows_the_input_gain_for_a_calibrated_level() {
        // calibrated at unity gain: -22 dBFS; +6 dB gain now puts the voice at -16 dBFS at the focus stage
        let s = MicSettings { voice_level_db: Some(-22.0), voice_focus_db: -14.0, input_gain_db: 6.0, ..MicSettings::default() };
        assert_eq!(DspParams::from_settings(&s).focus_threshold_db, -30.0);
        // the uncalibrated assumption is not moved by the gain
        let s = MicSettings { voice_level_db: None, ..s };
        assert_eq!(DspParams::from_settings(&s).focus_threshold_db, -40.0);
    }

    #[test]
    fn output_gain_is_applied_before_the_limiter() {
        let mut chain = MicChain::new(&DspParams { output_gain_db: 20.0, ..passthrough() });
        let ceiling = crate::dsp::db_to_lin(-1.0);
        let mut noise = Noise::new(6);
        let mut worst = 0.0f32;
        for _ in 0..200 {
            let mut buf: Vec<f32> = (0..FRAME).map(|_| noise.sample() * 0.5).collect(); // 10x gain -> far above full scale
            chain.process(&mut buf);
            worst = buf.iter().fold(worst, |w, s| w.max(s.abs()));
        }
        assert!(worst <= ceiling + 1e-6, "+20 dB output gain must stay under -1 dBFS, peak {worst}");
        assert!(worst > ceiling * 0.99, "and the limiter should be the thing holding it there, peak {worst}");
    }

    #[test]
    fn strength_maps_to_attenuation_limit() {
        assert!((atten_lim_db(0.0) - 6.0).abs() < 1e-4);
        assert!((atten_lim_db(1.0) - 100.0).abs() < 1e-3);
        assert!((atten_lim_db(0.5) - 24.49).abs() < 0.05);
        assert!(atten_lim_db(0.3) < atten_lim_db(0.6));
    }

    #[test]
    fn chain_with_focus_gain_and_eq_stays_finite_and_below_full_scale() {
        let p = DspParams {
            denoise: DenoiseMode::Light,
            voice_focus: true,
            hpf_on: true,
            auto_level: true,
            gate_on: true,
            eq: EqPreset::Broadcast,
            tone_db: [12.0, 12.0, -12.0],
            input_gain_db: 12.0,
            output_gain_db: 20.0,
            ..passthrough()
        };
        let mut chain = MicChain::new(&p);
        let mut noise = Noise::new(5);
        for _ in 0..300 {
            let mut buf: Vec<f32> = (0..FRAME).map(|_| noise.sample() * 0.9).collect();
            chain.process(&mut buf);
            assert!(buf.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
    }

    #[test]
    fn agc_learns_the_voice_through_rnnoise_and_follows_input_changes() {
        let p = DspParams { denoise: DenoiseMode::Light, auto_level: true, ..passthrough() };
        let mut chain = MicChain::new(&p);
        let (mut voice, mut noise) = (Voice::new(), Noise::new(0x5EED));
        // the selftest signal: 0.6 s of voice, 0.9 s of silence, over -35 dBFS noise
        let (mut speech_frames, mut speech_vad, mut run, mut best_run) = (0, 0, 0, 0);
        let mut known = None;
        for i in 0..1500 {
            let speech = i % 150 < 60;
            let mut buf: Vec<f32> = (0..FRAME)
                .map(|_| {
                    let v = voice.sample();
                    (if speech { v } else { 0.0 }) + noise.sample() * 0.03
                })
                .collect();
            let info = chain.process(&mut buf);
            if speech {
                speech_frames += 1;
                if info.vad >= SPEECH_VAD {
                    (speech_vad, run) = (speech_vad + 1, run + 1);
                    best_run = best_run.max(run);
                } else {
                    run = 0;
                }
            }
            if info.voice_db.is_some() {
                known = Some(info);
                break;
            }
        }
        let info = known.unwrap_or_else(|| {
            panic!("no voice level in 1500 frames: vad >= {SPEECH_VAD} on {speech_vad} of {speech_frames} speech frames, longest run {best_run}")
        });
        assert_eq!(
            info.voice_db,
            Some(info.input_voice_db.unwrap() + info.agc_gain_db),
            "voice_db = input + AGC gain + 0 dB output gain"
        );

        // A known change of the input moves the learned level at once. Silence first, so the detector's
        // speech tail is over and no frame teaches the estimate anything during the check.
        let mut quiet = |chain: &mut MicChain| {
            let mut buf: Vec<f32> = (0..FRAME).map(|_| noise.sample() * 0.03).collect();
            chain.process(&mut buf).input_voice_db.unwrap()
        };
        let mut input = 0.0;
        for _ in 0..30 {
            input = quiet(&mut chain);
        }
        chain.set_params(&DspParams { input_gain_db: 6.0, ..p });
        let up = quiet(&mut chain);
        assert!((up - (input + 6.0)).abs() < 0.01, "input gain +6 dB moved the level {input:.2} -> {up:.2}");
        chain.shift_input_db(-4.0);
        let down = quiet(&mut chain);
        assert!((down - (up - 4.0)).abs() < 0.01, "shift_input_db(-4) moved the level {up:.2} -> {down:.2}");
    }

    #[test]
    fn light_mode_reports_rnnoise_and_latency() {
        let chain = MicChain::new(&DspParams { denoise: DenoiseMode::Light, ..passthrough() });
        assert_eq!(chain.denoise_active(), DenoiseMode::Light);
        assert_eq!(chain.latency_ms(), RNNOISE_LATENCY_MS);
    }

    #[cfg(feature = "dfn")]
    #[test]
    fn strong_mode_builds_dfn_and_live_switching_works() {
        let mut chain = MicChain::new(&DspParams { denoise: DenoiseMode::Strong, ..passthrough() });
        assert_eq!(chain.denoise_active(), DenoiseMode::Strong, "DFN must initialise in the test profile");
        assert!((chain.latency_ms() - 30.0).abs() < 0.01);
        let mut noise = Noise::new(9);
        let mut buf: Vec<f32> = (0..FRAME).map(|_| noise.sample() * 0.1).collect();
        chain.process(&mut buf);
        chain.set_params(&DspParams { denoise: DenoiseMode::Light, denoise_strength: 0.3, ..passthrough() });
        assert_eq!(chain.denoise_active(), DenoiseMode::Light);
        chain.set_params(&DspParams { denoise: DenoiseMode::Strong, denoise_strength: 0.3, ..passthrough() });
        assert_eq!(chain.denoise_active(), DenoiseMode::Strong, "model is kept, no rebuild");
        chain.process(&mut buf);
        assert!(buf.iter().all(|s| s.is_finite()));
    }
}
