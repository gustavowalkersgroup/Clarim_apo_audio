//! Neural noise suppression via RNNoise (nnnoiseless, pure Rust). Also the engine's speech-probability
//! source in every denoise mode (see `chain`).

use super::{Block, FRAME};
use nnnoiseless::DenoiseState;

/// RNNoise expects 480-sample frames at 48 kHz with samples in i16 float range.
pub struct Denoise {
    state: Box<DenoiseState<'static>>,
    strength: f32, // dry/wet: 1.0 = fully denoised
    scaled_in: [f32; FRAME],
    scaled_out: [f32; FRAME],
    /// Unscaled input of the last two frames: RNNoise's output lags its input by exactly one frame
    /// (measured: 479-480 samples), so the dry part of a partial mix is the previous frame.
    dry_cur: [f32; FRAME],
    dry_prev: [f32; FRAME],
    pub last_vad: f32,
}

impl Denoise {
    pub fn new(strength: f32) -> Self {
        Self {
            state: DenoiseState::new(),
            strength: strength.clamp(0.0, 1.0),
            scaled_in: [0.0; FRAME],
            scaled_out: [0.0; FRAME],
            dry_cur: [0.0; FRAME],
            dry_prev: [0.0; FRAME],
            last_vad: 0.0,
        }
    }

    pub fn set_strength(&mut self, strength: f32) {
        self.strength = strength.clamp(0.0, 1.0);
    }

    /// Run the network on one frame and return its speech probability (0..1). The denoised frame is
    /// kept internally for `mix_wet`; `input` is not modified. Allocation-free.
    pub fn analyze(&mut self, input: &[f32]) -> f32 {
        debug_assert_eq!(input.len(), FRAME);
        self.dry_prev = self.dry_cur;
        self.dry_cur.copy_from_slice(input);
        for (dst, &src) in self.scaled_in.iter_mut().zip(input.iter()) {
            *dst = src * 32768.0;
        }
        self.last_vad = self
            .state
            .process_frame(&mut self.scaled_out[..FRAME], &self.scaled_in[..FRAME]);
        self.last_vad
    }

    /// Replace `buf` (the frame last passed to `analyze`) by the dry/wet mix of the denoised frame.
    /// The dry signal is delayed by one frame to line up with the network's output (no comb filtering).
    pub fn mix_wet(&self, buf: &mut [f32]) {
        for ((out, &wet), &dry) in buf.iter_mut().zip(self.scaled_out.iter()).zip(self.dry_prev.iter()) {
            *out = (wet / 32768.0) * self.strength + dry * (1.0 - self.strength);
        }
    }
}

impl Block for Denoise {
    fn process(&mut self, buf: &mut [f32]) {
        self.analyze(buf);
        self.mix_wet(buf);
    }
    fn reset(&mut self) {
        self.state = DenoiseState::new();
        self.dry_cur = [0.0; FRAME];
        self.dry_prev = [0.0; FRAME];
        self.last_vad = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::Noise;

    #[test]
    fn dry_only_mix_is_the_input_delayed_by_one_frame() {
        let mut d = Denoise::new(0.0);
        let mut noise = Noise::new(21);
        let mut prev = [0.0f32; FRAME];
        for _ in 0..5 {
            let input: [f32; FRAME] = std::array::from_fn(|_| noise.sample() * 0.1);
            let mut buf = input;
            d.process(&mut buf);
            assert_eq!(buf, prev);
            prev = input;
        }
    }

    #[test]
    fn analyze_leaves_the_input_untouched_and_reports_a_probability() {
        let mut d = Denoise::new(1.0);
        let mut noise = Noise::new(22);
        let input: [f32; FRAME] = std::array::from_fn(|_| noise.sample() * 0.1);
        let vad = d.analyze(&input);
        assert!((0.0..=1.0).contains(&vad));
    }
}
