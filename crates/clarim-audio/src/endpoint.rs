//! Windows endpoint volume of a capture device (the "Microfone" level slider), in dB. The backend lowers it
//! when the mic clips and gives it back later (design: docs/leak-agc.md section 5.4).
//!
//! Called from the backend's housekeeping thread, never from the DSP thread.

use anyhow::{Context, Result};
use windows::core::{GUID, HSTRING};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, ENDPOINT_HARDWARE_SUPPORT_VOLUME};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};

/// Tags every volume change Clarim makes, so a listener on the endpoint can tell them from the user's.
const CLARIM_EVENT_GUID: GUID = GUID::from_u128(0x7c3f_1a52_9e4b_4d86_a1c7_5b2e_8f40_d319);

/// The master volume of an endpoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EndpointVolume {
    pub db: f32,
    pub min_db: f32,
    pub max_db: f32,
    /// The level is a real hardware control (analog gain). A software-only slider scales the samples after
    /// the converter, so it cannot stop the mic from clipping.
    pub hardware: bool,
}

/// Balances this thread's COM initialisation: `S_OK` and `S_FALSE` both count and are undone on drop;
/// `RPC_E_CHANGED_MODE` (the thread already runs another apartment model) is usable and left alone.
struct ComGuard(bool);

impl ComGuard {
    fn enter() -> Result<Self> {
        // SAFETY: plain COM initialisation of the calling thread; no pointers are involved.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self(true))
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self(false))
        } else {
            Err(anyhow::anyhow!("COM init failed: {hr:?}"))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx of `enter` on this same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// The volume interface of endpoint `device_id`. Every COM object it creates is dropped before the
/// caller's `ComGuard`, because the guard is declared first there.
fn open(device_id: &str) -> Result<IAudioEndpointVolume> {
    // SAFETY: COM calls on a thread the caller has initialised; the id string outlives the GetDevice call
    // and every returned interface is owned by a windows-rs smart pointer.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("device enumerator")?;
        let device: IMMDevice = enumerator.GetDevice(&HSTRING::from(device_id)).context("audio endpoint not found")?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).context("endpoint volume interface")
    }
}

fn read(volume: &IAudioEndpointVolume) -> Result<EndpointVolume> {
    let (mut min_db, mut max_db, mut step_db) = (0.0f32, 0.0f32, 0.0f32);
    // SAFETY: the out pointers are valid locals for the duration of each call.
    unsafe {
        volume.GetVolumeRange(&mut min_db, &mut max_db, &mut step_db).context("volume range")?;
        let db = volume.GetMasterVolumeLevel().context("master volume")?;
        let support = volume.QueryHardwareSupport().context("hardware support")?;
        Ok(EndpointVolume { db, min_db, max_db, hardware: support & ENDPOINT_HARDWARE_SUPPORT_VOLUME != 0 })
    }
}

/// The master volume, range and hardware flag of endpoint `device_id` (a `DeviceInfo::id`).
pub fn endpoint_volume(device_id: &str) -> Result<EndpointVolume> {
    let _com = ComGuard::enter()?;
    // A local, not a tail-expression temporary: in edition 2021 the temporary would be released after `_com`.
    let volume = open(device_id)?;
    read(&volume)
}

/// Set the master volume of endpoint `device_id` to `db` (clamped to its range, tagged with Clarim's event
/// context) and return the value read back: a device with coarse steps lands on the nearest one.
pub fn set_endpoint_volume_db(device_id: &str, db: f32) -> Result<f32> {
    anyhow::ensure!(db.is_finite(), "volume {db} is not a number");
    let _com = ComGuard::enter()?;
    let volume = open(device_id)?;
    let range = read(&volume)?;
    // SAFETY: the GUID pointer is valid for the call.
    unsafe {
        volume.SetMasterVolumeLevel(db.clamp(range.min_db, range.max_db), &CLARIM_EVENT_GUID).context("set master volume")?;
        volume.GetMasterVolumeLevel().context("read back master volume")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn com_guard_is_balanced_on_a_thread_that_already_has_com() {
        // a fresh thread: S_OK first, S_FALSE for the nested one; both are undone without a panic or leak
        std::thread::spawn(|| {
            let outer = ComGuard::enter().expect("first init");
            let inner = ComGuard::enter().expect("nested init");
            assert!(outer.0 && inner.0, "both S_OK and S_FALSE must be balanced");
            drop(inner);
            drop(outer);
            let again = ComGuard::enter().expect("init after a full cycle");
            assert!(again.0);
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn a_bad_id_is_an_error_not_a_panic() {
        assert!(endpoint_volume("no-such-endpoint").is_err());
        assert!(set_endpoint_volume_db("no-such-endpoint", -3.0).is_err());
        assert!(set_endpoint_volume_db("no-such-endpoint", f32::NAN).is_err());
    }

    /// Manual hardware check (read-only: nothing is set): prints the default microphone's volume, range and
    /// hardware flag.
    /// Run with `cargo test --release -p clarim-audio --lib -- --ignored endpoint_volume_of_default_mic --nocapture`.
    #[test]
    #[ignore = "reads the default microphone's Windows volume (read-only)"]
    fn endpoint_volume_of_default_mic() {
        crate::init_com().expect("COM");
        let inputs = crate::list_inputs(&[]).expect("list inputs");
        let mic = crate::pick_real_mic(&inputs).expect("no real microphone");
        let v = endpoint_volume(&mic.id).expect("read the endpoint volume");
        println!(
            "'{}': {:.1} dB (range {:.1} .. {:.1} dB), hardware volume control: {}",
            mic.name, v.db, v.min_db, v.max_db, v.hardware
        );
        assert!(v.min_db <= v.db && v.db <= v.max_db, "volume {} outside its range", v.db);
    }
}
