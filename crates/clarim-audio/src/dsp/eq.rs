//! RBJ cookbook biquads and the EQ presets built from them.

use super::{Block, SAMPLE_RATE};
use clarim_core::EqPreset;

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::identity()
    }
}

impl Biquad {
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn from_coeffs(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn peaking(freq: f32, gain_db: f32, q: f32) -> Self {
        let (w0, a) = wq(freq, gain_db);
        let alpha = w0.sin() / (2.0 * q as f64);
        let cosw = w0.cos();
        Self::from_coeffs(
            1.0 + alpha * a,
            -2.0 * cosw,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cosw,
            1.0 - alpha / a,
        )
    }

    pub fn low_shelf(freq: f32, gain_db: f32) -> Self {
        let (w0, a) = wq(freq, gain_db);
        let s: f64 = 1.0;
        let alpha = w0.sin() / 2.0 * ((a + 1.0 / a) * (1.0 / s - 1.0) + 2.0).sqrt();
        let cosw = w0.cos();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        Self::from_coeffs(
            a * ((a + 1.0) - (a - 1.0) * cosw + two_sqrt_a_alpha),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cosw),
            a * ((a + 1.0) - (a - 1.0) * cosw - two_sqrt_a_alpha),
            (a + 1.0) + (a - 1.0) * cosw + two_sqrt_a_alpha,
            -2.0 * ((a - 1.0) + (a + 1.0) * cosw),
            (a + 1.0) + (a - 1.0) * cosw - two_sqrt_a_alpha,
        )
    }

    pub fn high_shelf(freq: f32, gain_db: f32) -> Self {
        let (w0, a) = wq(freq, gain_db);
        let s: f64 = 1.0;
        let alpha = w0.sin() / 2.0 * ((a + 1.0 / a) * (1.0 / s - 1.0) + 2.0).sqrt();
        let cosw = w0.cos();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        Self::from_coeffs(
            a * ((a + 1.0) + (a - 1.0) * cosw + two_sqrt_a_alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cosw),
            a * ((a + 1.0) + (a - 1.0) * cosw - two_sqrt_a_alpha),
            (a + 1.0) - (a - 1.0) * cosw + two_sqrt_a_alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cosw),
            (a + 1.0) - (a - 1.0) * cosw - two_sqrt_a_alpha,
        )
    }

    pub fn high_pass(freq: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * freq as f64 / SAMPLE_RATE as f64;
        let alpha = w0.sin() / (2.0 * q as f64);
        let cosw = w0.cos();
        Self::from_coeffs(
            (1.0 + cosw) / 2.0,
            -(1.0 + cosw),
            (1.0 + cosw) / 2.0,
            1.0 + alpha,
            -2.0 * cosw,
            1.0 - alpha,
        )
    }

    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        // transposed direct form II
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn clear(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Take over `new`'s coefficients but keep the running state, so a live retune does not click.
    fn retune(&mut self, new: Biquad) {
        *self = Biquad { z1: self.z1, z2: self.z2, ..new };
    }
}

fn wq(freq: f32, gain_db: f32) -> (f64, f64) {
    let w0 = 2.0 * std::f64::consts::PI * freq as f64 / SAMPLE_RATE as f64;
    let a = 10.0_f64.powf(gain_db as f64 / 40.0);
    (w0, a)
}

impl Block for Biquad {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            *s = self.tick(*s);
        }
    }
    fn reset(&mut self) {
        self.clear();
    }
}

/// EQ preset as a fixed cascade of three biquads (unused sections are identity), so switching presets
/// live never allocates.
pub struct PresetEq {
    preset: EqPreset,
    sections: [Biquad; 3],
}

fn preset_sections(p: EqPreset) -> [Biquad; 3] {
    let id = Biquad::identity();
    match p {
        EqPreset::Flat => [id; 3],
        EqPreset::Clarity => [Biquad::high_shelf(4000.0, 3.0), Biquad::peaking(300.0, -2.0, 1.0), id],
        EqPreset::Warm => [Biquad::low_shelf(150.0, 2.5), Biquad::high_shelf(8000.0, -1.5), id],
        EqPreset::Broadcast => [
            Biquad::low_shelf(120.0, 2.0),
            Biquad::peaking(400.0, -3.0, 1.0),
            Biquad::peaking(3500.0, 3.0, 1.0),
        ],
    }
}

impl PresetEq {
    pub fn new(preset: EqPreset) -> Self {
        Self { preset, sections: preset_sections(preset) }
    }

    pub fn set_preset(&mut self, preset: EqPreset) {
        if preset == self.preset {
            return;
        }
        self.preset = preset;
        for (cur, new) in self.sections.iter_mut().zip(preset_sections(preset)) {
            cur.retune(new);
            if preset == EqPreset::Flat {
                cur.clear(); // the chain bypasses Flat, so stale state must not leak into the next preset
            }
        }
    }
}

impl Block for PresetEq {
    fn process(&mut self, buf: &mut [f32]) {
        for bq in &mut self.sections {
            bq.process(buf);
        }
    }
    fn reset(&mut self) {
        for bq in &mut self.sections {
            bq.clear();
        }
    }
}

/// Frames a `ToneEq` keeps running after all gains went to 0, so the filter state drains through the
/// (now unity) sections instead of being cut off with a click. A 150 Hz shelf rings for ~2 ms.
const TONE_TAIL_FRAMES: u32 = 10;

/// The three mic tone sliders `[bass, mid, treble]` in dB, run after the preset: low shelf 150 Hz,
/// peak 1.5 kHz Q 1, high shelf 6 kHz. Bypassed (bit-exact) while all three are 0. Gains must be finite
/// and already clamped (`DspParams` does that). A live `set` keeps the filter state, so it never clicks.
pub struct ToneEq {
    gains: [f32; 3],
    sections: [Biquad; 3],
    /// Calls of `process` still to run after the gains became all-zero.
    tail: u32,
}

fn tone_sections([bass, mid, treble]: [f32; 3]) -> [Biquad; 3] {
    [Biquad::low_shelf(150.0, bass), Biquad::peaking(1500.0, mid, 1.0), Biquad::high_shelf(6000.0, treble)]
}

impl ToneEq {
    pub fn new(gains: [f32; 3]) -> Self {
        Self { gains, sections: tone_sections(gains), tail: 0 }
    }

    pub fn set(&mut self, gains: [f32; 3]) {
        if gains == self.gains {
            return;
        }
        self.gains = gains;
        self.tail = if gains == [0.0; 3] { TONE_TAIL_FRAMES } else { 0 };
        for (cur, new) in self.sections.iter_mut().zip(tone_sections(gains)) {
            cur.retune(new);
        }
    }
}

impl Block for ToneEq {
    fn process(&mut self, buf: &mut [f32]) {
        if self.gains == [0.0; 3] {
            if self.tail == 0 {
                return;
            }
            self.tail -= 1;
        }
        for bq in &mut self.sections {
            bq.process(buf);
        }
    }
    fn reset(&mut self) {
        self.tail = 0;
        for bq in &mut self.sections {
            bq.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Steady-state gain in dB of `b` for a sine at `freq`.
    fn gain_db(b: &mut dyn Block, freq: f32) -> f32 {
        let n = 48_000;
        let x: Vec<f32> = (0..n).map(|i| (i as f32 / SAMPLE_RATE * freq * TAU).sin() * 0.25).collect();
        let mut y = x.clone();
        b.process(&mut y);
        let rms = |v: &[f32]| (v[n / 2..].iter().map(|s| s * s).sum::<f32>() / (n / 2) as f32).sqrt();
        20.0 * (rms(&y) / rms(&x)).log10()
    }

    #[test]
    fn flat_is_identity() {
        let mut eq = PresetEq::new(EqPreset::Flat);
        let x: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.37).sin() * 0.6).collect();
        let mut y = x.clone();
        eq.process(&mut y);
        assert!(x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn presets_hit_their_targets() {
        let mut clarity = PresetEq::new(EqPreset::Clarity);
        assert!(gain_db(&mut clarity, 12_000.0) > 2.5, "clarity high shelf");
        let mut clarity = PresetEq::new(EqPreset::Clarity);
        assert!((gain_db(&mut clarity, 300.0) + 2.0).abs() < 0.5, "clarity -2 dB at 300 Hz");
        let mut warm = PresetEq::new(EqPreset::Warm);
        assert!(gain_db(&mut warm, 50.0) > 2.0, "warm low shelf");
        let mut bc = PresetEq::new(EqPreset::Broadcast);
        assert!((gain_db(&mut bc, 3500.0) - 3.0).abs() < 0.5, "broadcast +3 dB at 3.5 kHz");
    }

    #[test]
    fn live_preset_switch_back_to_flat_is_identity_again() {
        let mut eq = PresetEq::new(EqPreset::Broadcast);
        let mut warmup = vec![0.1f32; 960];
        eq.process(&mut warmup);
        eq.set_preset(EqPreset::Flat);
        let x: Vec<f32> = (0..960).map(|i| (i as f32 * 0.21).sin() * 0.5).collect();
        let mut z = x.clone();
        eq.process(&mut z);
        assert!(x.iter().zip(&z).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn tone_is_unity_at_zero_db() {
        let mut tone = ToneEq::new([0.0; 3]);
        let x: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.37).sin() * 0.6).collect();
        let mut y = x.clone();
        tone.process(&mut y);
        assert_eq!(x, y, "all-zero is a bit-exact bypass");
        // a single 0 dB section is a pole/zero cancellation: still unity (this is what runs while only the
        // other two sliders are moved, and during the tail after a retune to 0)
        for (freq, mut bq) in [
            (80.0, Biquad::low_shelf(150.0, 0.0)),
            (1500.0, Biquad::peaking(1500.0, 0.0, 1.0)),
            (12_000.0, Biquad::high_shelf(6000.0, 0.0)),
        ] {
            assert!(gain_db(&mut bq, freq).abs() < 0.01, "{freq} Hz");
        }
    }

    #[test]
    fn tone_sliders_move_their_own_band_only() {
        let at = |gains: [f32; 3], freq: f32| gain_db(&mut ToneEq::new(gains), freq);
        // +6 dB bass: ~+6 dB at 80 Hz (below the 150 Hz corner), 3 kHz untouched
        assert!((at([6.0, 0.0, 0.0], 80.0) - 6.0).abs() < 1.0, "bass at 80 Hz: {}", at([6.0, 0.0, 0.0], 80.0));
        assert!(at([6.0, 0.0, 0.0], 3000.0).abs() < 0.5, "bass leaks into 3 kHz");
        // +6 dB mid: +6 dB at the 1.5 kHz centre, 80 Hz and 12 kHz untouched
        assert!((at([0.0, 6.0, 0.0], 1500.0) - 6.0).abs() < 0.5, "mid centre");
        assert!(at([0.0, 6.0, 0.0], 80.0).abs() < 0.5 && at([0.0, 6.0, 0.0], 12_000.0).abs() < 1.0, "mid leaks");
        // +6 dB treble: ~+6 dB at 12 kHz (above the 6 kHz corner), 3 kHz and 80 Hz barely moved
        assert!((at([0.0, 0.0, 6.0], 12_000.0) - 6.0).abs() < 1.0, "treble at 12 kHz: {}", at([0.0, 0.0, 6.0], 12_000.0));
        assert!(at([0.0, 0.0, 6.0], 80.0).abs() < 0.5 && at([0.0, 0.0, 6.0], 1500.0).abs() < 0.8, "treble leaks");
        // cuts work the same way
        assert!(at([-6.0, 0.0, 0.0], 80.0) < -4.5);
    }

    #[test]
    fn tone_live_retune_does_not_click() {
        // +12 dB bass on an 80 Hz tone, then the slider jumps to 0: the output must glide to the dry tone
        // (a bypass that dropped the filter state would step by roughly the state size, ~0.5)
        let mut tone = ToneEq::new([12.0, 0.0, 0.0]);
        let sine = |n: usize| (n as f32 / SAMPLE_RATE * 80.0 * TAU).sin() * 0.2;
        let mut prev = 0.0f32;
        let mut worst = 0.0f32;
        for frame in 0..60 {
            if frame == 30 {
                tone.set([0.0; 3]);
            }
            let mut buf: Vec<f32> = (0..480).map(|k| sine(frame * 480 + k)).collect();
            tone.process(&mut buf);
            for &s in &buf {
                worst = worst.max((s - prev).abs());
                prev = s;
            }
        }
        // the loudest legitimate slope is the boosted tone itself: 0.8 * 2*pi*80/48000 = 0.0084
        assert!(worst < 0.012, "step of {worst} between samples");
    }
}
