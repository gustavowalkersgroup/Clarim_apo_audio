//! Clarim mic engine: WASAPI capture -> DSP (HPF, denoise, voice focus, EQ, leveling) -> virtual mic.
//!
//! Internal format: 48 kHz mono f32, 480-sample frames (10 ms).

pub mod calib;
pub mod chain;
pub mod devices;
pub mod dsp;
mod endpoint;
mod engine;
mod pacing;
pub mod pipeline;
mod recovery;
mod refs;
mod selftest;

use anyhow::Result;
use clarim_core::DeviceInfo;
use devices::Flow;

pub use devices::{find_virtual_mic, pick_real_mic};
pub use dsp::agc::{AGC_MAX_GAIN_DB, AGC_MIN_GAIN_DB, AGC_TARGET_DB};
pub use endpoint::*;
pub use engine::{default_render_id, virtual_mic_capture, MicEngine, NO_VIRTUAL_MIC};
pub use pipeline::Counters;
pub use recovery::{is_audio_service_loss, is_reconnecting, RECONNECTING};
pub use selftest::selftest;

/// COM (MTA) init helper for threads that enumerate or open devices.
/// Safe to call more than once per thread; returns Err if the thread
/// already runs STA COM (e.g. the GUI thread - use a worker instead).
pub fn init_com() -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|e| anyhow::anyhow!("COM init failed: {e}"))
}

/// Active capture endpoints minus the deny-list. The calling thread needs COM (`init_com`).
pub fn list_inputs(deny: &[String]) -> Result<Vec<DeviceInfo>> {
    devices::enumerate(Flow::Capture, deny)
}

/// Active render endpoints minus the deny-list (virtual ones included, flagged).
/// The calling thread needs COM (`init_com`).
pub fn list_outputs(deny: &[String]) -> Result<Vec<DeviceInfo>> {
    devices::enumerate(Flow::Render, deny)
}
