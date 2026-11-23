//! Dynamics and level blocks: gain, gate, compressor, limiter.

use super::{db_to_lin, lin_to_db, onepole_coef, Block, SAMPLE_RATE};

/// Smoothed gain.
pub struct Gain {
    target: f32,
    current: f32,
    coef: f32,
}

impl Gain {
    pub fn new(db: f32) -> Self {
        let g = db_to_lin(db);
        Self {
            target: g,
            current: g,
            coef: onepole_coef(8.0),
        }
    }
    pub fn set_db(&mut self, db: f32) {
        self.target = db_to_lin(db);
    }
}

impl Block for Gain {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            self.current = self.target + (self.current - self.target) * self.coef;
            *s *= self.current;
        }
    }
}

/// Classic downward noise gate with hold.
pub struct NoiseGate {
    pub threshold_lin: f32,
    attack: f32,
    release: f32,
    env_coef: f32,
    hold_samples: u32,
    env: f32,
    gain: f32,
    hold_ctr: u32,
}

impl NoiseGate {
    pub fn new(threshold_db: f32, attack_ms: f32, release_ms: f32, hold_ms: f32) -> Self {
        Self {
            threshold_lin: db_to_lin(threshold_db),
            attack: onepole_coef(attack_ms),
            release: onepole_coef(release_ms),
            env_coef: onepole_coef(2.0),
            hold_samples: (hold_ms * 0.001 * SAMPLE_RATE) as u32,
            env: 0.0,
            gain: 0.0,
            hold_ctr: 0,
        }
    }
    pub fn set_threshold_db(&mut self, db: f32) {
        self.threshold_lin = db_to_lin(db);
    }
}

impl Block for NoiseGate {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            let a = s.abs();
            self.env = if a > self.env {
                a
            } else {
                a + (self.env - a) * self.env_coef
            };
            let open = self.env > self.threshold_lin;
            if open {
                self.hold_ctr = self.hold_samples;
            } else if self.hold_ctr > 0 {
                self.hold_ctr -= 1;
            }
            let target = if open || self.hold_ctr > 0 { 1.0 } else { 0.0 };
            let coef = if target > self.gain {
                self.attack
            } else {
                self.release
            };
            self.gain = target + (self.gain - target) * coef;
            *s *= self.gain;
        }
    }
    fn reset(&mut self) {
        self.env = 0.0;
        self.gain = 0.0;
        self.hold_ctr = 0;
    }
}

/// Feed-forward compressor with program-style envelope in dB domain.
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack: f32,
    release: f32,
    makeup: f32,
    env_db: f32,
}

impl Compressor {
    pub fn new(threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32, makeup_db: f32) -> Self {
        Self {
            threshold_db,
            ratio: ratio.max(1.0),
            attack: onepole_coef(attack_ms),
            release: onepole_coef(release_ms),
            makeup: db_to_lin(makeup_db),
            env_db: -90.0,
        }
    }
}

impl Block for Compressor {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            let level_db = lin_to_db(s.abs());
            let coef = if level_db > self.env_db {
                self.attack
            } else {
                self.release
            };
            self.env_db = level_db + (self.env_db - level_db) * coef;
            let over = self.env_db - self.threshold_db;
            // below the threshold (most samples of a quiet mic) only the makeup applies: no powf
            *s *= if over > 0.0 {
                db_to_lin(over * (1.0 / self.ratio - 1.0)) * self.makeup
            } else {
                self.makeup
            };
        }
    }
    fn reset(&mut self) {
        self.env_db = -90.0;
    }
}

/// Hard safety limiter (fast peak follower driving gain reduction).
pub struct Limiter {
    ceiling: f32,
    env: f32,
    release: f32,
}

impl Limiter {
    pub fn new(ceiling_db: f32) -> Self {
        Self {
            ceiling: db_to_lin(ceiling_db),
            env: 0.0,
            release: onepole_coef(80.0),
        }
    }
}

impl Block for Limiter {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            let a = s.abs();
            self.env = if a > self.env {
                a
            } else {
                a + (self.env - a) * self.release
            };
            if self.env > self.ceiling {
                *s *= self.ceiling / self.env;
            }
            // hard guarantee against float rounding: never above the ceiling
            *s = s.clamp(-self.ceiling, self.ceiling);
        }
    }
    fn reset(&mut self) {
        self.env = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::Noise;

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let ceiling = db_to_lin(-1.0);
        let mut lim = Limiter::new(-1.0);
        let mut noise = Noise::new(3);
        let mut worst = 0.0f32;
        for n in 0..2000 {
            // bursts up to +20 dB over full scale, then quiet stretches
            let amp = if n % 50 < 10 { 10.0 } else { 0.1 };
            let mut buf: Vec<f32> = (0..480).map(|_| noise.sample() * amp).collect();
            lim.process(&mut buf);
            worst = buf.iter().fold(worst, |w, s| w.max(s.abs()));
        }
        assert!(worst <= ceiling, "peak {worst} above ceiling {ceiling}");
        assert!(worst > ceiling * 0.99, "limiter should actually reach the ceiling, got {worst}");
    }

    #[test]
    fn limiter_leaves_quiet_signal_untouched() {
        let mut lim = Limiter::new(-1.0);
        let dry: Vec<f32> = (0..480).map(|i| ((i as f32) * 0.05).sin() * 0.5).collect();
        let mut buf = dry.clone();
        lim.process(&mut buf);
        assert_eq!(buf, dry);
    }

    #[test]
    fn compressor_applies_makeup_below_threshold_and_compresses_above() {
        let mut c = Compressor::new(-24.0, 3.0, 10.0, 200.0, 6.0);
        let mut quiet = vec![0.001f32; 4800]; // -60 dBFS DC: far below threshold
        c.process(&mut quiet);
        assert!((quiet[4799] / 0.001 - db_to_lin(6.0)).abs() < 1e-3);
        let mut loud = vec![0.5f32; 48_000]; // -6 dBFS: 18 dB over -> 12 dB reduction, +6 makeup = -6 dB
        c.process(&mut loud);
        let g = loud[47_999] / 0.5;
        assert!((g - db_to_lin(-6.0)).abs() < 0.01, "gain {g}");
    }
}
