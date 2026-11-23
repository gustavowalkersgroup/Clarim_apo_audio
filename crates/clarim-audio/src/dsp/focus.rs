//! Voice Focus: a downward expander keyed on the (post-denoise) RMS level and gated by speech probability.
//!
//! The user's mouth is typically 15-25 dB louder at the mic than people talking across the room, so
//! "level above (calibrated voice level + a negative offset) AND speech detected" passes the user and
//! turns the rest down by `FLOOR_DB` (not to silence: a floor avoids pumping artifacts).
//! It cannot tell two voices at the same level apart; it is a level gate, not a speaker model.

use super::{db_to_lin, onepole_coef};

/// Gain when closed.
pub const FLOOR_DB: f32 = -30.0;
/// Speech probability needed to open.
const VAD_OPEN: f32 = 0.5;
const HOLD_SAMPLES: u32 = 12_000; // 250 ms @ 48 kHz

pub struct VoiceFocus {
    /// Mean-square envelope (attack ~5 ms, release ~120 ms).
    env: f32,
    env_attack: f32,
    env_release: f32,
    threshold: f32, // mean-square (linear power) threshold
    hold_left: u32,
    gain: f32,
    floor: f32,
    gain_attack: f32,
    gain_release: f32,
}

impl VoiceFocus {
    pub fn new(threshold_db: f32) -> Self {
        let mut f = Self {
            env: 0.0,
            env_attack: onepole_coef(5.0),
            env_release: onepole_coef(120.0),
            threshold: 0.0,
            hold_left: 0,
            gain: 1.0,
            floor: db_to_lin(FLOOR_DB),
            gain_attack: onepole_coef(3.0),
            gain_release: onepole_coef(80.0),
        };
        f.set_threshold_db(threshold_db);
        f
    }

    /// RMS threshold in dBFS (calibrated voice level + the user's offset).
    pub fn set_threshold_db(&mut self, db: f32) {
        self.threshold = 10.0_f32.powf(db / 10.0);
    }

    /// Process one frame. `vad` is the speech probability of this frame. When `enabled` is false the
    /// expander is forced open, so a live switch-off fades back to unity instead of clicking, and once
    /// it is back at unity the frame is left untouched. Returns whether audio is currently passing
    /// because of speech (always false when disabled).
    pub fn process(&mut self, buf: &mut [f32], vad: f32, enabled: bool) -> bool {
        // The f32 one-pole stalls a few 1e-6 short of 1.0, so "back at unity" means within 0.001 dB.
        if !enabled && self.gain > 0.9999 {
            self.gain = 1.0;
            return false;
        }
        let speech = vad > VAD_OPEN;
        for s in buf.iter_mut() {
            let p = *s * *s;
            let c = if p > self.env { self.env_attack } else { self.env_release };
            self.env = p + (self.env - p) * c;
            if enabled && speech && self.env > self.threshold {
                self.hold_left = HOLD_SAMPLES;
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
            }
            let target = if !enabled || self.hold_left > 0 { 1.0 } else { self.floor };
            let c = if target > self.gain { self.gain_attack } else { self.gain_release };
            self.gain = target + (self.gain - target) * c;
            *s *= self.gain;
        }
        enabled && self.hold_left > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{FRAME, SAMPLE_RATE};
    use std::f32::consts::TAU;

    /// 150 Hz + 450 Hz tone pair scaled to the given RMS in dBFS.
    fn tone(i: usize, rms_db: f32) -> f32 {
        let t = i as f32 / SAMPLE_RATE;
        let raw = ((t * 150.0 * TAU).sin() + (t * 450.0 * TAU).sin() * 0.5) / (1.25f32 / 2.0).sqrt();
        raw * db_to_lin(rms_db)
    }

    fn rms_db(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).max(1e-12).log10()
    }

    /// Run `frames` frames at `level_db`; returns (input, output) RMS dB over the last 50 frames.
    fn run(f: &mut VoiceFocus, start: &mut usize, frames: usize, level_db: f32, vad: f32) -> (f32, f32) {
        let (mut ins, mut outs) = (Vec::new(), Vec::new());
        for n in 0..frames {
            let mut buf: Vec<f32> = (0..FRAME).map(|k| tone(*start + k, level_db)).collect();
            let dry = buf.clone();
            f.process(&mut buf, vad, true);
            *start += FRAME;
            if n >= frames - 50 {
                ins.extend(dry);
                outs.extend(buf);
            }
        }
        (rms_db(&ins), rms_db(&outs))
    }

    #[test]
    fn loud_passes_and_minus_20_db_is_attenuated() {
        // threshold between the two levels: -26 dBFS speech vs -46 dBFS "background", threshold -40
        let mut f = VoiceFocus::new(-40.0);
        let mut pos = 0;
        let (i, o) = run(&mut f, &mut pos, 200, -26.0, 1.0);
        assert!((o - i).abs() < 0.5, "loud signal should pass: {i:.1} -> {o:.1} dB");
        let (i, o) = run(&mut f, &mut pos, 300, -46.0, 1.0);
        assert!(i - o >= 15.0, "quiet signal should be attenuated by >= 15 dB, got {:.1}", i - o);
    }

    #[test]
    fn no_speech_closes_even_when_loud_and_hold_keeps_it_open() {
        let mut f = VoiceFocus::new(-40.0);
        let mut pos = 0;
        let (i, o) = run(&mut f, &mut pos, 200, -20.0, 0.0);
        assert!(i - o >= 15.0, "loud non-speech must be attenuated, got {:.1}", i - o);
        // speech opens it, then 100 ms without speech is still inside the 250 ms hold
        run(&mut f, &mut pos, 100, -20.0, 1.0);
        let (mut ins, mut outs) = (Vec::new(), Vec::new());
        for n in 0..10 {
            let mut buf: Vec<f32> = (0..FRAME).map(|k| tone(pos + n * FRAME + k, -20.0)).collect();
            ins.extend(buf.clone());
            f.process(&mut buf, 0.0, true);
            outs.extend(buf);
        }
        assert!((rms_db(&outs) - rms_db(&ins)).abs() < 1.0);
    }

    #[test]
    fn disabled_returns_to_unity_and_then_bypasses() {
        let mut f = VoiceFocus::new(-40.0);
        let mut pos = 0;
        run(&mut f, &mut pos, 200, -60.0, 0.0); // closed
        for _ in 0..100 {
            let mut buf: [f32; FRAME] = std::array::from_fn(|k| tone(pos + k, -30.0));
            pos += FRAME;
            assert!(!f.process(&mut buf, 0.0, false));
        }
        let dry: [f32; FRAME] = std::array::from_fn(|k| tone(pos + k, -30.0));
        let mut out = dry;
        f.process(&mut out, 0.0, false);
        assert_eq!(out, dry, "bit-exact bypass once back at unity");
    }
}
