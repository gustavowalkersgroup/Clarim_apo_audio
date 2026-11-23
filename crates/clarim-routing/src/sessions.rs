//! WASAPI session enumeration over every ACTIVE render endpoint, plus pid -> exe resolution.
//!
//! COM objects here are created, used and dropped on the thread that initialised COM ([`ComGuard`]).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;

use anyhow::{anyhow, Result};
use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    eCapture, eRender, AudioSessionStateActive, AudioSessionStateExpired, EDataFlow, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Audio plumbing, not apps: Windows' audio engine host, SteelSeries Sonar's engine (it renders the
/// final mix, with sessions on every real device) and NVIDIA Broadcast. Moving or re-volumeing
/// them would break other audio software, so they are never listed, moved or re-volumed.
const SYSTEM_EXES: [&str; 3] = ["audiodg.exe", "steelseriesggclient.exe", "nvidia broadcast.exe"];

/// `exe` of a session whose process image could not be resolved (access denied, gone). Never routed.
pub const UNKNOWN_EXE: &str = "unknown";

/// Initialises COM (MTA) for the current thread and undoes it on drop.
pub(crate) struct ComGuard(bool);

impl ComGuard {
    pub(crate) fn init() -> Result<Self> {
        // SAFETY: plain per-thread COM initialisation, balanced by CoUninitialize in Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            // The thread is already an STA: COM is usable, but it is not ours to uninitialise.
            return Ok(Self(false));
        }
        hr.ok().map_err(|e| anyhow!("CoInitializeEx: {e}"))?;
        Ok(Self(true))
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx in `init` on this same thread.
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn device_enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: COM is initialised on this thread by the caller (ComGuard).
    unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| anyhow!("MMDeviceEnumerator: {e}"))
}

/// Plain data about one audio session (no COM objects).
#[derive(Clone, Debug)]
pub(crate) struct SessionInfo {
    pub pid: u32,
    /// Lowercase file name, or [`UNKNOWN_EXE`].
    pub exe: String,
    pub name: String,
    /// Render endpoint (`IMMDevice::GetId()`) the session lives on.
    pub device_id: String,
    /// Session instance identifier; unique per session.
    pub key: String,
    pub active: bool,
    pub peak: f32,
}

/// A session plus the COM objects used to meter and control it. Worker-thread only.
pub(crate) struct Live {
    pub info: SessionInfo,
    pub vol: ISimpleAudioVolume,
    pub meter: IAudioMeterInformation,
}

pub(crate) struct Scan {
    /// Ids of all active render endpoints.
    pub endpoints: Vec<String>,
    pub sessions: Vec<Live>,
}

/// Enumerate every non-expired app session on every active render endpoint, skipping the system
/// sounds session, pid 0, `own_pid` and [`SYSTEM_EXES`]. `exes` caches pid -> exe and gets entries for new pids.
pub(crate) fn enumerate(
    en: &IMMDeviceEnumerator,
    own_pid: u32,
    exes: &mut HashMap<u32, String>,
) -> Result<Scan> {
    let mut scan = Scan { endpoints: Vec::new(), sessions: Vec::new() };
    // SAFETY: COM is initialised on this thread (callers hold a ComGuard); every interface obtained
    // here is either dropped before returning or moved into the returned Scan.
    unsafe {
        for_each_endpoint(en, eRender, |dev, id| {
            scan.endpoints.push(id.to_owned());
            let sessions = &mut scan.sessions;
            for_each_session(dev, id, |ctl, i| match live_session(ctl, id, i, own_pid, exes) {
                Ok(Some(s)) => sessions.push(s),
                Ok(None) => {}
                Err(e) => log::debug!("routing: session {i} on {id}: {e}"),
            });
        })?;
    }
    Ok(scan)
}

/// A microphone (capture) session of an app. Read-only data.
#[derive(Clone, Debug)]
pub(crate) struct CaptureInfo {
    pub pid: u32,
    /// Lowercase file name, or [`UNKNOWN_EXE`].
    pub exe: String,
    /// Capture endpoint (`IMMDevice::GetId()`) the session lives on.
    pub device_id: String,
    pub active: bool,
}

/// Every non-expired app session on every active CAPTURE endpoint, with the same skips as
/// [`enumerate`]. Opens no stream and writes nothing.
pub(crate) fn enumerate_capture(
    en: &IMMDeviceEnumerator,
    own_pid: u32,
    exes: &mut HashMap<u32, String>,
) -> Result<Vec<CaptureInfo>> {
    let mut out = Vec::new();
    // SAFETY: COM is initialised on this thread (callers hold a ComGuard); every interface obtained
    // here is dropped before returning.
    unsafe {
        for_each_endpoint(en, eCapture, |dev, id| {
            for_each_session(dev, id, |ctl, i| match capture_session(ctl, id, own_pid, exes) {
                Ok(Some(c)) => out.push(c),
                Ok(None) => {}
                Err(e) => log::debug!("routing: capture session {i} on {id}: {e}"),
            });
        })?;
    }
    Ok(out)
}

/// Calls `f(endpoint, endpoint_id)` for every ACTIVE endpoint of `flow`.
///
/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn for_each_endpoint(
    en: &IMMDeviceEnumerator,
    flow: EDataFlow,
    mut f: impl FnMut(&IMMDevice, &str),
) -> Result<()> {
    let devices = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE).map_err(|e| anyhow!("EnumAudioEndpoints: {e}"))?;
    let count = devices.GetCount().map_err(|e| anyhow!("device count: {e}"))?;
    for i in 0..count {
        let Ok(dev) = devices.Item(i) else { continue };
        let Ok(id) = dev.GetId().map(take_pwstr) else { continue };
        f(&dev, &id);
    }
    Ok(())
}

/// Calls `f(session, index)` for every session of `dev`; a device that cannot be queried is logged and skipped.
///
/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn for_each_session(dev: &IMMDevice, device_id: &str, mut f: impl FnMut(&IAudioSessionControl, i32)) {
    let list = match session_list(dev) {
        Ok(l) => l,
        Err(e) => return log::debug!("routing: sessions of endpoint {device_id}: {e}"),
    };
    let Ok(count) = list.GetCount() else { return };
    for i in 0..count {
        if let Ok(ctl) = list.GetSession(i) {
            f(&ctl, i);
        }
    }
}

/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn session_list(dev: &IMMDevice) -> Result<IAudioSessionEnumerator> {
    let manager: IAudioSessionManager2 =
        dev.Activate(CLSCTX_ALL, None).map_err(|e| anyhow!("activate session manager: {e}"))?;
    manager.GetSessionEnumerator().map_err(|e| anyhow!("GetSessionEnumerator: {e}"))
}

/// First skips shared by render and capture scans: system sounds, pid 0 and `own_pid` give `Ok(None)`.
///
/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn session_pid(ctl: &IAudioSessionControl, own_pid: u32) -> Result<Option<(IAudioSessionControl2, u32)>> {
    let ctl2: IAudioSessionControl2 = ctl.cast().map_err(|e| anyhow!("IAudioSessionControl2: {e}"))?;
    if ctl2.IsSystemSoundsSession() == S_OK {
        return Ok(None);
    }
    // AUDCLNT_S_NO_SINGLE_PROCESS is a success code, so GetProcessId's Result is Ok for it too.
    let pid = ctl2.GetProcessId().map_err(|e| anyhow!("GetProcessId: {e}"))?;
    Ok((pid != 0 && pid != own_pid).then_some((ctl2, pid)))
}

/// Cached lowercase exe of `pid`; `None` for audio plumbing ([`SYSTEM_EXES`]).
fn app_exe(pid: u32, exes: &mut HashMap<u32, String>) -> Option<String> {
    let exe = exes.entry(pid).or_insert_with(|| exe_name(pid));
    (!SYSTEM_EXES.contains(&exe.as_str())).then(|| exe.clone())
}

/// `Ok(None)` = deliberately skipped (see [`session_pid`], expired, system exes).
///
/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn capture_session(
    ctl: &IAudioSessionControl,
    device_id: &str,
    own_pid: u32,
    exes: &mut HashMap<u32, String>,
) -> Result<Option<CaptureInfo>> {
    let Some((_, pid)) = session_pid(ctl, own_pid)? else { return Ok(None) };
    let state = ctl.GetState().map_err(|e| anyhow!("GetState: {e}"))?;
    if state == AudioSessionStateExpired {
        return Ok(None);
    }
    let Some(exe) = app_exe(pid, exes) else { return Ok(None) };
    Ok(Some(CaptureInfo { pid, exe, device_id: device_id.to_owned(), active: state == AudioSessionStateActive }))
}

/// `Ok(None)` = deliberately skipped (see [`session_pid`], expired, system exes).
///
/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn live_session(
    ctl: &IAudioSessionControl,
    device_id: &str,
    index: i32,
    own_pid: u32,
    exes: &mut HashMap<u32, String>,
) -> Result<Option<Live>> {
    let Some((ctl2, pid)) = session_pid(ctl, own_pid)? else { return Ok(None) };
    let state = ctl.GetState().map_err(|e| anyhow!("GetState: {e}"))?;
    if state == AudioSessionStateExpired {
        return Ok(None);
    }
    let Some(exe) = app_exe(pid, exes) else { return Ok(None) };
    let name = display_name(&ctl.GetDisplayName().map(take_pwstr).unwrap_or_default(), &exe);
    let key = ctl2
        .GetSessionInstanceIdentifier()
        .map(take_pwstr)
        .unwrap_or_else(|_| format!("{device_id}/{pid}/{index}"));
    let vol: ISimpleAudioVolume = ctl.cast().map_err(|e| anyhow!("ISimpleAudioVolume: {e}"))?;
    let meter: IAudioMeterInformation = ctl.cast().map_err(|e| anyhow!("IAudioMeterInformation: {e}"))?;
    let peak = meter.GetPeakValue().unwrap_or(0.0);
    let info = SessionInfo {
        pid,
        exe,
        name,
        device_id: device_id.to_owned(),
        key,
        active: state == AudioSessionStateActive,
        peak,
    };
    Ok(Some(Live { info, vol, meter }))
}

/// Session display name when it is a plain string, else the exe name without `.exe`.
/// Windows apps often use resource strings (`@%SystemRoot%\...`) which are not displayable.
pub(crate) fn display_name(raw: &str, exe: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('@') {
        exe.strip_suffix(".exe").unwrap_or(exe).to_owned()
    } else {
        raw.to_owned()
    }
}

/// Copies and CoTaskMemFree's a string returned by a WASAPI getter.
fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: WASAPI getters return a NUL-terminated wide string allocated with CoTaskMemAlloc,
    // owned by the caller; it is read once and freed exactly once here.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// Closes a process handle on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful OpenProcess and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Lowercase image file name of `pid`, or [`UNKNOWN_EXE`] (access denied, process gone).
fn exe_name(pid: u32) -> String {
    let path = match image_path(pid) {
        Ok(p) => p,
        Err(e) => {
            log::debug!("routing: cannot resolve exe of pid {pid}: {e}");
            return UNKNOWN_EXE.to_owned();
        }
    };
    Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| UNKNOWN_EXE.to_owned())
}

fn image_path(pid: u32) -> Result<String> {
    let mut buf = [0u16; 2048];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is closed by `Handle`; `buf` is valid for `len` u16s and the API updates
    // `len` to the number of characters written (without the NUL).
    unsafe {
        let h = Handle(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(|e| anyhow!("OpenProcess: {e}"))?,
        );
        QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
            .map_err(|e| anyhow!("QueryFullProcessImageNameW: {e}"))?;
    }
    Ok(String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_falls_back_to_exe_stem() {
        assert_eq!(display_name("", "spotify.exe"), "spotify");
        assert_eq!(display_name("   ", "spotify.exe"), "spotify");
        assert_eq!(display_name(r"@%SystemRoot%\System32\AudioSrv.Dll,-203", "svchost.exe"), "svchost");
        assert_eq!(display_name("Spotify Premium", "spotify.exe"), "Spotify Premium");
        assert_eq!(display_name("", UNKNOWN_EXE), UNKNOWN_EXE);
    }

    /// Read-only: lists the raw capture (microphone) and render sessions on this machine, any state.
    /// Opens no stream and writes nothing.
    /// `rtk proxy cargo test -p clarim-routing live_sessions -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the live audio sessions of this machine"]
    fn live_sessions_are_listed() {
        let _com = ComGuard::init().expect("com");
        let en = device_enumerator().expect("enumerator");
        // Unfiltered view first (also shows what the filters skip), then what the engine uses.
        // SAFETY: COM is initialised above; the interfaces live only inside the closures.
        unsafe {
            for (flow, label) in [(eCapture, "capture"), (eRender, "render")] {
                for_each_endpoint(&en, flow, |dev, id| {
                    println!("{label} endpoint {id}");
                    for_each_session(dev, id, |ctl, i| {
                        let ctl2: IAudioSessionControl2 = ctl.cast().expect("IAudioSessionControl2");
                        let (pid, state) = (ctl2.GetProcessId().unwrap_or(0), ctl.GetState().map(|s| s.0).unwrap_or(-1));
                        println!(
                            "  raw session {i}: pid {pid} exe {} state {state} (0 inactive, 1 active, 2 expired) system_sounds={}",
                            exe_name(pid),
                            ctl2.IsSystemSoundsSession() == S_OK
                        );
                    });
                })
                .expect("endpoints");
            }
        }
        let caps = enumerate_capture(&en, std::process::id(), &mut HashMap::new()).expect("capture sessions");
        println!("{} capture sessions after filtering", caps.len());
        for c in &caps {
            println!("pid {:>6} {:<28} active={} on {}", c.pid, c.exe, c.active, c.device_id);
        }
    }

    #[test]
    fn own_exe_resolves_and_bogus_pid_is_unknown() {
        let exe = exe_name(std::process::id());
        assert!(exe.ends_with(".exe") && exe == exe.to_lowercase(), "{exe}");
        assert_eq!(exe_name(0xFFFF_FFF0), UNKNOWN_EXE);
    }
}
