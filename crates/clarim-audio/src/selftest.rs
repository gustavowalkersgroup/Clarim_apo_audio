//! `clarim selftest`: synthetic speech + noise through the full mic chain in every denoise mode
//! (Voice Focus on), no devices touched. Reports the cost per 10 ms frame and the levels.

use crate::chain::{DspParams, MicChain, ASSUMED_VOICE_LEVEL_DB};
use crate::dsp::{Noise, FRAME, SAMPLE_RATE};
use anyhow::{bail, Result};
use clarim_core::{DenoiseMode, EqPreset};
use std::f32::consts::TAU;
use std::fmt::Write;
use std::time::Instant;

const CYCLE: usize = 150; // speech frames 0..60, then noise only
const FRAMES: usize = CYCLE * 4; // 6 s
/// Frames excluded from the timing figures (model warm-up, cold caches).
const WARMUP_FRAMES: usize = 20;
/// In each cycle speech runs on frames 10..55 (interior, away from edges and model delay)...
const SPEECH_FRAMES: std::ops::Range<usize> = 10..55;
/// ...and the noise-only level is measured from frame 100 on: the models delay audio (DFN3: 3 frames)
/// and release slowly, and Voice Focus holds open for 25 frames after the last speech frame.
const QUIET_FROM: usize = 100;
/// Measured on this signal: RNNoise about 32 dB, DFN3 far more. A denoiser that cannot manage 10 dB is broken.
const MIN_DENOISE_DROP_DB: f64 = 10.0;

/// Synthetic "speech": a 120 Hz voiced tone (slow pitch wobble, 8 harmonics) with a 4 Hz syllable
/// envelope, around -25 dBFS RMS.
pub(crate) struct Voice {
    phase: f32,
    t: f32,
}

impl Voice {
    pub(crate) fn new() -> Self {
        Self { phase: 0.0, t: 0.0 }
    }

    pub(crate) fn sample(&mut self) -> f32 {
        let syllable = 0.1 + 0.9 * (0.5 - 0.5 * (self.t * 4.0 * TAU).cos());
        let tone: f32 = (1..=8).map(|k| (self.phase * k as f32 * TAU).sin() / k as f32).sum();
        let f0 = 120.0 + 10.0 * (self.t * 3.0 * TAU).sin();
        self.phase = (self.phase + f0 / SAMPLE_RATE).fract();
        self.t = (self.t + 1.0 / SAMPLE_RATE).fract();
        tone * syllable * 0.1
    }
}

/// Voice for 0.6 s and silence for 0.9 s per cycle, over white noise at about -35 dBFS.
fn synth_frames() -> Vec<[f32; FRAME]> {
    let mut noise = Noise::new(0x5EED);
    let mut voice = Voice::new();
    (0..FRAMES)
        .map(|i| {
            let speech = i % CYCLE < 60;
            std::array::from_fn(|_| {
                let v = voice.sample(); // keeps running through the pauses; only gated in
                (if speech { v } else { 0.0 }) + noise.sample() * 0.03
            })
        })
        .collect()
}

fn db(power: f64) -> f64 {
    10.0 * power.max(1e-12).log10()
}

#[derive(Default)]
struct Level {
    in_pow: f64,
    out_pow: f64,
    n: usize,
}

impl Level {
    fn add(&mut self, input: &[f32], output: &[f32]) {
        self.in_pow += input.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        self.out_pow += output.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        self.n += input.len();
    }
    fn in_db(&self) -> f64 {
        db(self.in_pow / self.n.max(1) as f64)
    }
    fn out_db(&self) -> f64 {
        db(self.out_pow / self.n.max(1) as f64)
    }
}

struct Stats {
    avg_us: f64,
    max_us: f64,
    speech: Level,
    noise: Level,
    /// Share of speech frames in which Voice Focus was passing audio.
    focus_open: f64,
    active: DenoiseMode,
}

/// Run the synthetic signal through a fresh chain; errors on non-finite or out-of-range samples.
fn measure(frames: &[[f32; FRAME]], params: &DspParams) -> Result<Stats> {
    let mut chain = MicChain::new(params);
    let (mut spent, mut max_us, mut timed) = (0.0f64, 0.0f64, 0usize);
    let (mut speech, mut noise) = (Level::default(), Level::default());
    let (mut open_frames, mut speech_frames) = (0usize, 0usize);
    for (i, input) in frames.iter().enumerate() {
        let mut buf = *input;
        let t0 = Instant::now();
        let info = chain.process(&mut buf);
        let us = t0.elapsed().as_secs_f64() * 1e6;
        if buf.iter().any(|s| !s.is_finite() || s.abs() > 1.0) {
            bail!("non-finite or out-of-range output at frame {i}");
        }
        if i >= WARMUP_FRAMES {
            spent += us;
            max_us = max_us.max(us);
            timed += 1;
            let pos = i % CYCLE;
            if SPEECH_FRAMES.contains(&pos) {
                speech.add(input, &buf);
                speech_frames += 1;
                open_frames += usize::from(info.focus_open);
            } else if pos >= QUIET_FROM {
                noise.add(input, &buf);
            }
        }
    }
    Ok(Stats {
        avg_us: spent / timed.max(1) as f64,
        max_us,
        speech,
        noise,
        focus_open: open_frames as f64 / speech_frames.max(1) as f64,
        active: chain.denoise_active(),
    })
}

pub fn selftest() -> Result<String> {
    let frames = synth_frames();
    let mut report = format!(
        "selftest: {FRAMES} frames of {FRAME} samples @ 48 kHz (10 ms each, budget 10000 us), full chain, Voice Focus on\n\
         levels are RMS dBFS input -> output; the chain adds +6 dB auto-level makeup and Voice Focus mutes non-speech by 30 dB\n"
    );
    let mut failed = false;

    for (name, mode) in [
        ("off", DenoiseMode::Off),
        ("light (RNNoise)", DenoiseMode::Light),
        ("strong (DeepFilterNet3)", DenoiseMode::Strong),
    ] {
        if mode == DenoiseMode::Strong && !cfg!(feature = "dfn") {
            writeln!(report, "{name:<24} skipped: built without the `dfn` feature")?;
            continue;
        }
        let full = DspParams {
            denoise: mode,
            denoise_strength: 1.0,
            voice_focus: true,
            focus_threshold_db: ASSUMED_VOICE_LEVEL_DB - 14.0,
            gate_on: false,
            gate_threshold_db: -50.0,
            hpf_on: true,
            auto_level: true,
            eq: EqPreset::Flat,
            tone_db: [0.0; 3],
            input_gain_db: 0.0,
            output_gain_db: 0.0,
        };
        // Same chain with only the denoiser active: what the denoiser alone does to noise-only frames.
        let denoise_only = DspParams {
            voice_focus: false,
            hpf_on: false,
            auto_level: false,
            ..full
        };
        let result = (|| -> Result<String> {
            let build = Instant::now();
            drop(MicChain::new(&full)); // model build time (DFN3 only)
            let build_ms = build.elapsed().as_secs_f64() * 1e3;
            let st = measure(&frames, &full)?;
            if st.active != mode {
                bail!("requested {mode:?} but {:?} is running (denoiser unavailable)", st.active);
            }
            let only = measure(&frames, &denoise_only)?;
            let drop_db = only.noise.in_db() - only.noise.out_db();
            if mode != DenoiseMode::Off && drop_db < MIN_DENOISE_DROP_DB {
                bail!("denoiser alone lowered the noise-only level by {drop_db:.1} dB, expected at least {MIN_DENOISE_DROP_DB:.0} dB");
            }
            Ok(format!(
                "{name:<24} avg {:>6.1} us  max {:>6.1} us  speech {:>6.1} -> {:>6.1}  noise-only {:>6.1} -> {:>6.1}  \
                 denoise alone {:>+6.1} dB on noise  focus open {:>3.0}% of speech  (chain build {build_ms:.0} ms)",
                st.avg_us,
                st.max_us,
                st.speech.in_db(),
                st.speech.out_db(),
                st.noise.in_db(),
                st.noise.out_db(),
                -drop_db,
                st.focus_open * 100.0,
            ))
        })();
        match result {
            Ok(line) => writeln!(report, "{line}")?,
            Err(e) => {
                failed = true;
                writeln!(report, "FAILED: {name}: {e:#}")?;
            }
        }
    }

    if failed {
        bail!("{report}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest_runs_every_mode() {
        let report = super::selftest().unwrap();
        println!("{report}");
        assert!(report.contains("off") && report.contains("light"));
        assert!(report.contains("strong (DeepFilterNet3)") && !report.contains("FAILED"));
    }
}
