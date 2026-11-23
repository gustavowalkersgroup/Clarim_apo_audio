//! Voice-level calibration: while the user speaks, collect the RMS (dBFS, post-denoise) of every frame
//! the speech detector calls speech, then take the 70th percentile as "the user's voice level".
//! Voice Focus opens above that level plus a negative offset.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const MAX_SECONDS: f32 = 30.0;
/// Fewer speech frames than this (0.5 s) and the result is not trustworthy.
pub const MIN_SPEECH_FRAMES: usize = 50;
pub const SPEECH_VAD: f32 = 0.6;
const FRAMES_PER_SECOND: f32 = 100.0;
const PERCENTILE: f32 = 0.70;
/// Bit pattern of "no result" (a NaN, never a real dB value).
const NO_RESULT: u32 = u32::MAX;

/// Lock-free mailbox between the control thread (`MicEngine`) and the DSP thread.
#[derive(Debug)]
pub struct CalibShared {
    request_frames: AtomicU32,
    calibrating: AtomicBool,
    progress: AtomicU32, // f32 bits
    result: AtomicU32,   // f32 bits or NO_RESULT
    failed: AtomicBool,
}

impl CalibShared {
    pub fn new() -> Self {
        Self {
            request_frames: AtomicU32::new(0),
            calibrating: AtomicBool::new(false),
            progress: AtomicU32::new(0f32.to_bits()),
            result: AtomicU32::new(NO_RESULT),
            failed: AtomicBool::new(false),
        }
    }

    /// Control thread: ask the DSP thread to calibrate for `seconds` (clamped to 1..=30).
    pub fn request(&self, seconds: f32) {
        let frames = (seconds.clamp(1.0, MAX_SECONDS) * FRAMES_PER_SECOND) as u32;
        self.result.store(NO_RESULT, Ordering::Relaxed);
        self.failed.store(false, Ordering::Relaxed);
        self.progress.store(0f32.to_bits(), Ordering::Relaxed);
        self.calibrating.store(true, Ordering::Relaxed);
        self.request_frames.store(frames, Ordering::Release);
    }

    /// DSP thread: frames of a freshly requested calibration, once.
    pub fn take_request(&self) -> Option<u32> {
        match self.request_frames.swap(0, Ordering::Acquire) {
            0 => None,
            n => Some(n),
        }
    }

    /// DSP thread: report progress (0..1) of the running calibration.
    pub fn set_progress(&self, p: f32) {
        self.progress.store(p.to_bits(), Ordering::Relaxed);
    }

    /// DSP thread: a calibration ended. `None` = too little speech.
    pub fn finish(&self, result: Option<f32>) {
        match result {
            Some(db) => self.result.store(db.to_bits(), Ordering::Release),
            None => self.failed.store(true, Ordering::Release),
        }
        self.progress.store(1f32.to_bits(), Ordering::Relaxed);
        self.calibrating.store(false, Ordering::Release);
    }

    /// Abort without a result (engine stopped mid-calibration).
    pub fn cancel(&self) {
        self.request_frames.store(0, Ordering::Relaxed);
        self.calibrating.store(false, Ordering::Release);
    }

    pub fn calibrating(&self) -> bool {
        self.calibrating.load(Ordering::Acquire)
    }

    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    /// `Some(dBFS)` once after a successful calibration.
    pub fn take_result(&self) -> Option<f32> {
        match self.result.swap(NO_RESULT, Ordering::Acquire) {
            NO_RESULT => None,
            bits => Some(f32::from_bits(bits)),
        }
    }

    /// True once after a calibration that captured too little speech.
    pub fn take_failed(&self) -> bool {
        self.failed.swap(false, Ordering::Acquire)
    }
}

impl Default for CalibShared {
    fn default() -> Self {
        Self::new()
    }
}

/// DSP-thread side: the preallocated sample buffer and the frame counter. Never allocates after `new`.
pub struct Calibrator {
    buf: Vec<f32>,
    n: usize,
    left: u32,
    total: u32,
}

impl Calibrator {
    pub fn new() -> Self {
        Self {
            buf: vec![0.0; (MAX_SECONDS * FRAMES_PER_SECOND) as usize],
            n: 0,
            left: 0,
            total: 1,
        }
    }

    pub fn start(&mut self, frames: u32) {
        self.n = 0;
        self.total = frames.max(1);
        self.left = self.total;
    }

    pub fn active(&self) -> bool {
        self.left > 0
    }

    pub fn progress(&self) -> f32 {
        1.0 - self.left as f32 / self.total as f32
    }

    /// Feed one frame (its RMS in dBFS and speech probability). Returns `Some(result)` on the frame
    /// that ends the calibration: `Some(db)` or `None` if fewer than `MIN_SPEECH_FRAMES` were speech.
    pub fn feed(&mut self, rms_db: f32, vad: f32) -> Option<Option<f32>> {
        if self.left == 0 {
            return None;
        }
        if vad > SPEECH_VAD && self.n < self.buf.len() {
            self.buf[self.n] = rms_db;
            self.n += 1;
        }
        self.left -= 1;
        if self.left > 0 {
            return None;
        }
        if self.n < MIN_SPEECH_FRAMES {
            return Some(None);
        }
        let speech = &mut self.buf[..self.n];
        speech.sort_unstable_by(f32::total_cmp);
        Some(Some(percentile(speech, PERCENTILE)))
    }
}

impl Default for Calibrator {
    fn default() -> Self {
        Self::new()
    }
}

/// Linear-interpolated percentile (`p` in 0..1) of an ascending, non-empty slice.
pub fn percentile(sorted: &[f32], p: f32) -> f32 {
    let pos = p.clamp(0.0, 1.0) * (sorted.len() - 1) as f32;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_of_known_distribution() {
        let v: Vec<f32> = (1..=100).map(|x| x as f32).collect();
        assert!((percentile(&v, 0.70) - 70.3).abs() < 1e-4);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
        assert_eq!(percentile(&[-20.0], 0.7), -20.0);
    }

    #[test]
    fn calibrator_uses_only_speech_frames() {
        let mut c = Calibrator::new();
        c.start(500);
        let mut result = None;
        for i in 0..500 {
            // 200 speech frames at -40..-21 dBFS (20 distinct levels x 10), 300 noise frames at -60
            let (db, vad) = if i % 5 < 2 { (-40.0 + ((i / 5) % 20) as f32, 0.9) } else { (-60.0, 0.1) };
            if let Some(r) = c.feed(db, vad) {
                result = Some(r);
            }
        }
        let db = result.expect("calibration ended").expect("enough speech");
        // speech levels are uniform on -40..-21; the 70th percentile lands near -27
        assert!((-28.5..-25.5).contains(&db), "got {db}");
        assert!(!c.active());
    }

    #[test]
    fn too_little_speech_fails() {
        let mut c = Calibrator::new();
        c.start(300);
        let mut result = None;
        for i in 0..300 {
            let vad = if i < 49 { 0.9 } else { 0.1 }; // 49 < MIN_SPEECH_FRAMES
            if let Some(r) = c.feed(-30.0, vad) {
                result = Some(r);
            }
        }
        assert_eq!(result, Some(None));
    }

    #[test]
    fn mailbox_hands_over_exactly_once() {
        let s = CalibShared::new();
        s.request(5.0);
        assert!(s.calibrating());
        assert_eq!(s.take_request(), Some(500));
        assert_eq!(s.take_request(), None);
        s.finish(Some(-24.5));
        assert!(!s.calibrating());
        assert_eq!(s.take_result(), Some(-24.5));
        assert_eq!(s.take_result(), None);
        s.request(1.0);
        s.finish(None);
        assert!(s.take_failed());
        assert!(!s.take_failed());
        assert_eq!(s.take_result(), None);
    }
}
