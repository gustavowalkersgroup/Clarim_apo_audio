pub mod agc;
pub mod basic;
#[cfg(feature = "dfn")]
pub mod dfn;
pub mod denoise;
pub mod eq;
pub mod focus;
pub mod leak;

pub const SAMPLE_RATE: f32 = 48_000.0;
pub const FRAME: usize = 480; // 10 ms @ 48 kHz, matches RNNoise and DeepFilterNet3 hop size

/// A mono DSP block processing 48 kHz f32 buffers in place.
/// `process` is called with FRAME-sized buffers and must not allocate.
pub trait Block: Send {
    fn process(&mut self, buf: &mut [f32]);
    fn reset(&mut self) {}
}

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[inline]
pub fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-9).log10()
}

/// Mean square (linear power) to dB, floored at -90 dB (also for 0 and NaN).
#[inline]
pub fn ms_to_db(ms: f32) -> f32 {
    10.0 * ms.max(1e-9).log10()
}

/// One-pole smoothing coefficient for a time constant in ms.
#[inline]
pub fn onepole_coef(ms: f32) -> f32 {
    if ms <= 0.0 {
        return 0.0;
    }
    (-1.0 / (ms * 0.001 * SAMPLE_RATE)).exp()
}

/// Tiny deterministic white-noise source (xorshift), real-time safe.
#[derive(Clone)]
pub struct Noise {
    state: u32,
}

impl Noise {
    pub fn new(seed: u32) -> Self {
        Self {
            state: seed.max(1),
        }
    }
    #[inline]
    pub fn sample(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        // map to [-1, 1)
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}
