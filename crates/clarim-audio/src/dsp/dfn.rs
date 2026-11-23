//! DeepFilterNet3 ("strong" denoise) through libDF's tract backend, embedded default model.
//!
//! `DfTract` is NOT `Send` (it holds `Rc` tensors and `Box<dyn OpState>`), so `Dfn` is not a
//! `Block` (those are `Send`): build it on the thread that will run it, i.e. the DSP thread.
//!
//! libDF and tract still contain `unwrap`/`expect` calls, so construction and every frame run under
//! `catch_unwind`: a panic becomes an `Err` and the caller drops the model and falls back to RNNoise
//! instead of silently killing the DSP thread.

use super::FRAME;
use anyhow::{bail, ensure, Result};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use std::panic::{catch_unwind, AssertUnwindSafe};

pub struct Dfn {
    model: DfTract,
    out: [f32; FRAME],
    /// Last local SNR estimate in dB (libDF's per-frame speech/noise indicator; not used for the VAD).
    pub last_lsnr: f32,
}

impl Dfn {
    /// Builds the model from the embedded weights. Not cheap (~0.5 s): do it once per engine start,
    /// never on the audio callback path. `atten_lim_db` >= 100 means no attenuation limit.
    pub fn new(atten_lim_db: f32) -> Result<Self> {
        match catch_unwind(|| Self::build(atten_lim_db)) {
            Ok(res) => res,
            Err(_) => bail!("DeepFilterNet panicked while building the model"),
        }
    }

    fn build(atten_lim_db: f32) -> Result<Self> {
        let rp = RuntimeParams::default_with_ch(1).with_atten_lim(atten_lim_db);
        let model = DfTract::new(DfParams::default(), &rp)?;
        ensure!(
            model.sr == 48_000 && model.hop_size == FRAME,
            "unexpected DeepFilterNet geometry: sr {} hop {}",
            model.sr,
            model.hop_size
        );
        Ok(Self {
            model,
            out: [0.0; FRAME],
            last_lsnr: 0.0,
        })
    }

    pub fn set_atten_lim(&mut self, db: f32) {
        self.model.set_atten_lim(db);
    }

    /// Algorithmic delay of the model in ms: (fft - hop + lookahead * hop) samples, 30 ms for DFN3.
    pub fn latency_ms(&self) -> f32 {
        let m = &self.model;
        (m.fft_size - m.hop_size + m.lookahead * m.hop_size) as f32 / 48.0
    }

    /// Enhance one FRAME-sized buffer in place. On error the buffer is left untouched and the
    /// model should be discarded (after a panic its state is unknown).
    pub fn try_process(&mut self, buf: &mut [f32]) -> Result<()> {
        match catch_unwind(AssertUnwindSafe(|| self.process_inner(buf))) {
            Ok(res) => res,
            Err(_) => bail!("DeepFilterNet panicked while processing a frame"),
        }
    }

    fn process_inner(&mut self, buf: &mut [f32]) -> Result<()> {
        let noisy = ArrayView2::from_shape((1, FRAME), &*buf)?;
        let enhanced = ArrayViewMut2::from_shape((1, FRAME), &mut self.out)?;
        self.last_lsnr = self.model.process(noisy, enhanced)?;
        buf.copy_from_slice(&self.out);
        Ok(())
    }
}
