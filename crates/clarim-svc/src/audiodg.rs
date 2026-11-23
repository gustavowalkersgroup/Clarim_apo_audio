//! A look at the running Windows audio graph hosts (`audiodg.exe`): when each started and whether
//! `EqualizerAPO.dll` is loaded in it. Read-only.
//!
//! Why: an APO attached to an endpoint only takes effect in an `audiodg.exe` that started after the registry
//! change, so "this audiodg is older than the change" is exactly "Windows Audio must restart". The module
//! check (`apo_loaded`) is a second opinion for the dashboard. Whether a LocalSystem service may list the
//! modules of `audiodg.exe` is UNVERIFIED (it should, unless the process is protected, and it is not while
//! `DisableProtectedAudioDG` is 1); when it cannot, `apo_loaded` is `None` and only the start times decide.

use std::ffi::OsString;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows::core::PWSTR;
use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::FOLDERID_System;

/// What `inspect` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioDg {
    /// Creation time (FILETIME ticks) of every running `audiodg.exe`; `None` = the process list was unreadable.
    pub starts: Option<Vec<u64>>,
    /// `Some(true)` = at least one has the APO loaded, `Some(false)` = all could be read and none has,
    /// `None` = none running, or one could not be inspected.
    pub apo_loaded: Option<bool>,
}

/// The module check's verdict from what was seen per process (`Some(loaded)`, `None` = could not look).
pub(crate) fn loaded_verdict(per_process: &[Option<bool>]) -> Option<bool> {
    if per_process.contains(&Some(true)) {
        Some(true)
    } else if !per_process.is_empty() && per_process.iter().all(|p| *p == Some(false)) {
        Some(false)
    } else {
        None
    }
}

fn own(handle: HANDLE) -> OwnedHandle {
    // SAFETY: the caller passes a handle that was just returned by a successful call and nobody else owns.
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

/// Image path and creation time of `pid`, `None` when the process cannot be opened.
fn describe(pid: u32) -> Option<(OsString, u64)> {
    // SAFETY: plain open call; the handle is wrapped at once.
    let process = own(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let handle = HANDLE(process.as_raw_handle());
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is valid for `len` units and `len` is a valid in/out pointer.
    unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    let image = OsString::from_wide(&buf[..len as usize]);
    let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: the handle has query access and the four out pointers are valid.
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some((image, crate::apo_reg::filetime(created)))
}

/// Does process `pid` have a module called `EqualizerAPO.dll`? `None` = could not enumerate its modules.
fn has_apo_module(pid: u32) -> Option<bool> {
    // The documented retry: a snapshot of a process that is still loading can fail with ERROR_BAD_LENGTH.
    let mut snapshot = None;
    for _ in 0..3 {
        // SAFETY: plain query call; the handle is wrapped at once.
        if let Ok(h) = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) } {
            snapshot = Some(own(h));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let snapshot = snapshot?;
    let handle = HANDLE(snapshot.as_raw_handle());
    let mut entry = MODULEENTRY32W { dwSize: size_of::<MODULEENTRY32W>() as u32, ..Default::default() };
    // SAFETY: `entry.dwSize` is set and `snapshot` outlives the loop.
    let mut more = unsafe { Module32FirstW(handle, &mut entry) }.is_ok();
    while more {
        let len = entry.szModule.iter().position(|&c| c == 0).unwrap_or(entry.szModule.len());
        if String::from_utf16_lossy(&entry.szModule[..len]).eq_ignore_ascii_case("EqualizerAPO.dll") {
            return Some(true);
        }
        // SAFETY: same as Module32FirstW above.
        more = unsafe { Module32NextW(handle, &mut entry) }.is_ok();
    }
    Some(false)
}

pub(crate) fn inspect() -> AudioDg {
    let Some(system) = crate::known_folder(&FOLDERID_System) else { return AudioDg::default() };
    let expected = system.join("audiodg.exe");
    let mut starts = Vec::new();
    let mut loaded = Vec::new();
    let mut unreadable = false;
    for pid in crate::pids_named("audiodg.exe") {
        // Only the real one: a process of that name from elsewhere says nothing about Windows Audio.
        match describe(pid) {
            Some((image, created)) if image.eq_ignore_ascii_case(expected.as_os_str()) => {
                starts.push(created);
                loaded.push(has_apo_module(pid));
            }
            Some(_) => {}
            None => {
                // exists but cannot be opened (a protected process): its start time is unknown too
                loaded.push(None);
                unreadable = true;
            }
        }
    }
    AudioDg { starts: (!unreadable).then_some(starts), apo_loaded: loaded_verdict(&loaded) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_verdict_needs_every_process_to_have_been_read() {
        assert_eq!(loaded_verdict(&[]), None, "no audiodg running: cannot tell");
        assert_eq!(loaded_verdict(&[Some(true)]), Some(true));
        assert_eq!(loaded_verdict(&[Some(false), Some(true)]), Some(true));
        assert_eq!(loaded_verdict(&[Some(false), Some(false)]), Some(false));
        assert_eq!(loaded_verdict(&[Some(false), None]), None, "one unreadable process could be the one that has it");
        assert_eq!(loaded_verdict(&[None, Some(true)]), Some(true));
    }

    /// Read-only: what this PC's audio graph hosts look like to the current user (a normal user usually
    /// cannot open `audiodg.exe`; the service runs as SYSTEM, which is what is UNVERIFIED).
    /// `cargo test -p clarim-svc live_audiodg -- --ignored --nocapture`
    #[test]
    #[ignore = "inspects real processes"]
    fn live_audiodg() {
        println!("{:?}", inspect());
    }
}
