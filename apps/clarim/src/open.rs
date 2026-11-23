//! Opens the dashboard as an Edge app window (no tabs or address bar), falling back to the
//! default browser.
//!
//! Edge is started against the user's normal profile, with no private `--user-data-dir`: a fresh
//! profile signs in with the Windows account on its own, syncs the account's extensions and pops up
//! their welcome pages (seen on this machine), and nothing here may touch the user's account.
//!
//! The process is started through `ShellExecuteW` rather than `std::process::Command`: Command
//! lets the child inherit every inheritable handle of the caller, so a caller waiting on a pipe
//! (`clarim open | ...`, or cargo capturing test output) would hang until the window closes.

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Opens `http://127.0.0.1:<port>/`: Edge app window if Edge is installed, else the default browser.
pub fn open_dashboard(port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/");
    match find_edge() {
        Some(edge) => {
            let args = edge_arguments(&url);
            match shell_execute(edge.as_os_str(), Some(OsStr::new(&args))) {
                Ok(()) => return Ok(()),
                Err(e) => log::warn!("cannot start {}: {e}; using the default browser", edge.display()),
            }
        }
        None => log::info!("Edge not found; using the default browser"),
    }
    shell_execute(OsStr::new(&url), None)
}

/// Edge's command line (without the exe): an app window, no profile or sync switches.
fn edge_arguments(url: &str) -> String {
    format!("--app={url} --window-size=1180,820")
}

/// msedge.exe from the usual Program Files folders, else from the App Paths registry key.
fn find_edge() -> Option<PathBuf> {
    const REL: &str = r"Microsoft\Edge\Application\msedge.exe";
    ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|dir| PathBuf::from(dir).join(REL))
        .chain(edge_from_app_paths())
        .find(|p| p.is_file())
}

fn edge_from_app_paths() -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let mut bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: the key and subkey strings are static and NUL-terminated; `buf` is valid for `bytes`
    // bytes and `bytes` is a valid in/out pointer. A null value name reads the key's default value.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe"),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let path = String::from_utf16_lossy(&buf[..len]);
    Some(PathBuf::from(path.trim_matches('"')))
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// `ShellExecuteW(open, file, params)`: starts an exe, or opens a URL in the default browser.
fn shell_execute(file: &OsStr, params: Option<&OsStr>) -> Result<()> {
    let file = wide(file);
    let params = params.map(wide);
    // SAFETY: every string is NUL-terminated and outlives the call; no window handle is passed.
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(file.as_ptr()),
            params.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW reports success with a value greater than 32, an error code otherwise.
    let code = result.0 as usize;
    if code > 32 {
        Ok(())
    } else {
        Err(anyhow!("ShellExecuteW failed with code {code}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_arguments_use_the_normal_profile() {
        let args = edge_arguments("http://127.0.0.1:47800/");
        assert_eq!(args, "--app=http://127.0.0.1:47800/ --window-size=1180,820");
        // a private profile would sign in to the Windows account and sync extensions on its own
        for banned in ["--user-data-dir", "--disable-sync", "--disable-extensions", "--no-first-run"] {
            assert!(!args.contains(banned), "{banned} must not be passed: {args}");
        }
    }

    /// Read-only: whatever Edge lookup finds must be a real msedge.exe.
    #[test]
    fn found_edge_is_a_file() {
        if let Some(edge) = find_edge() {
            assert!(edge.is_file());
            assert!(edge.ends_with("msedge.exe"));
        }
    }

    /// The registry fallback alone must resolve to a real msedge.exe when the key exists.
    #[test]
    fn app_paths_lookup_is_a_file_when_present() {
        if let Some(edge) = edge_from_app_paths() {
            assert!(edge.is_file(), "{}", edge.display());
        }
    }

    /// Opens a real browser window, so it only runs on request:
    /// `cargo test -p clarim open_dashboard_launches -- --ignored`
    #[test]
    #[ignore]
    fn open_dashboard_launches() {
        open_dashboard(clarim_core::DEFAULT_PORT).expect("open_dashboard");
    }
}
