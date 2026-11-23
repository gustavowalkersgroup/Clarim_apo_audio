//! Probes the DeepFilterNet3 git dependency: embedded model builds, geometry is 48 kHz / 480 hop,
//! output stays finite on noise, and the cost per frame is measured.
//! Run in release for meaningful timings: `cargo test -p clarim-audio --release -- --nocapture`.
#![cfg(feature = "dfn")]

use clarim_audio::dsp::{Noise, FRAME};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use std::time::Instant;

#[test]
fn deepfilternet3_embedded_model_runs_at_48k_hop_480() {
    // DfTract is !Send (Rc tensors, Box<dyn OpState>): an `assert_send::<DfTract>()` here does not
    // compile. Build and use it on a single thread, the DSP thread.
    let t0 = Instant::now();
    let mut model = DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(1)).unwrap();
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;

    assert_eq!(model.sr, 48_000);
    assert_eq!(model.hop_size, 480);
    let delay_samples = model.fft_size - model.hop_size + model.lookahead * model.hop_size;
    println!(
        "DFN3: sr {} hop {} fft {} lookahead {} (df {}, conv {}) nb_erb {} nb_df {} df_order {}",
        model.sr,
        model.hop_size,
        model.fft_size,
        model.lookahead,
        model.df_lookahead,
        model.conv_lookahead,
        model.nb_erb,
        model.nb_df,
        model.df_order
    );
    println!(
        "DFN3: algorithmic delay {} samples = {:.1} ms; model build {:.0} ms",
        delay_samples,
        delay_samples as f64 / 48.0,
        build_ms
    );

    let mut noise = Noise::new(7);
    let mut run = |frames: usize| -> (f64, f64) {
        let (mut total_us, mut worst_us) = (0.0f64, 0.0f64);
        for _ in 0..frames {
            let mut noisy = [0.0f32; FRAME];
            for s in noisy.iter_mut() {
                *s = noise.sample() * 0.1;
            }
            let mut enhanced = [0.0f32; FRAME];
            let t = Instant::now();
            let lsnr = model
                .process(
                    ArrayView2::from_shape((1, FRAME), &noisy).unwrap(),
                    ArrayViewMut2::from_shape((1, FRAME), &mut enhanced).unwrap(),
                )
                .unwrap();
            let us = t.elapsed().as_secs_f64() * 1e6;
            total_us += us;
            worst_us = worst_us.max(us);
            assert!(lsnr.is_finite(), "lsnr {lsnr}");
            assert!(enhanced.iter().all(|s| s.is_finite()), "non-finite output");
        }
        (total_us / frames as f64, worst_us)
    };

    let (warm_avg, warm_worst) = run(10);
    println!("DFN3: first 10 frames avg {warm_avg:.0} us, worst {warm_worst:.0} us");
    let (avg, worst) = run(100);
    println!("DFN3: 100 frames of noise: avg {avg:.0} us/frame, worst {worst:.0} us (budget 10000 us)");
}
