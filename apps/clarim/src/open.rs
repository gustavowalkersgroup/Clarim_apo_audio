//! Opens the dashboard: in Clarim's own window (`clarim window`, `window.rs`), which is a separate process, and
//! when that cannot even be started, as an Edge app window (no tabs or address bar), falling back to the default
//! browser. `window.rs` itself falls back to the same browser path when WebView2 is missing.
//!
//! Edge is started against the user's normal profile, with no private `--user-data-dir`: a fresh
//! profile signs in with the Windows account on its own, syncs the account's extensions and pops up
//! their welcome pages (seen on this machine), and nothing here may touch the user's account.
//!
//! No process is started through `std::process::Command`: it lets the child inherit every inheritable handle of
//! the caller, so a caller waiting on a pipe (`clarim open | ...`, or cargo capturing test output) would hang until
//! the window closes. Edge and the browser go through `ShellExecuteW`; our own exe through `CreateProcessW` without
//! handle inheritance, because `ShellExecuteW` would ask "Open File - Security Warning" on every start when the exe
//! still carries the internet-zone mark of its download (the installer copies it with that mark).

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::{CreateProcessW, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, SW_SHOWNORMAL};

pub fn dashboard_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

/// Opens the dashboard in Clarim's own window; if that process cannot be started, in the browser (`open_in_browser`).
pub fn open_dashboard(port: u16) -> Result<()> {
    match start_window() {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("cannot start the dashboard window: {e:#}; using the browser");
            open_in_browser(&dashboard_url(port))
        }
    }
}

/// Starts `clarim window` (this exe) detached: no inherited handles, no console. The caller usually holds the right to
/// bring a window to the front (it was just clicked in the tray); passing it to the child lets the window come up on top.
fn start_window() -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    // A Windows path has no quote, so quoting it whole is enough.
    spawn_detached(&format!("\"{}\" window", exe.display())).map(drop)
}

/// `CreateProcessW` on `command_line` with no inherited handles; returns the new process id and does not wait for it.
fn spawn_detached(command_line: &str) -> Result<u32> {
    // CreateProcessW may write into the command line, so it must be an owned, mutable buffer.
    let mut cmd: Vec<u16> = command_line.encode_utf16().chain(Some(0)).collect();
    let startup = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `cmd` is a live NUL-terminated buffer and `startup` / `info` outlive the call; bInheritHandles is false,
    // no environment or directory is given (the child inherits both from this process).
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup,
            &mut info,
        )
    }
    .context("CreateProcessW")?;
    // SAFETY: CreateProcessW succeeded, so both handles are new and only we own them; dropping closes them.
    let (_process, _thread) = unsafe { (OwnedHandle::from_raw_handle(info.hProcess.0), OwnedHandle::from_raw_handle(info.hThread.0)) };
    // SAFETY: plain call with the new process id. It fails (ignored) when this process may not set the foreground.
    let _ = unsafe { AllowSetForegroundWindow(info.dwProcessId) };
    Ok(info.dwProcessId)
}

/// Opens `url` as an Edge app window if Edge is installed, else in the default browser.
pub fn open_in_browser(url: &str) -> Result<()> {
    match find_edge() {
        Some(edge) => {
            let args = edge_arguments(url);
            match shell_execute(edge.as_os_str(), Some(OsStr::new(&args))) {
                Ok(()) => return Ok(()),
                Err(e) => log::warn!("cannot start {}: {e}; using the default browser", edge.display()),
            }
        }
        None => log::info!("Edge not found; using the default browser"),
    }
    shell_execute(OsStr::new(url), None)
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

pub(crate) fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// `ShellExecuteW(open, file, params)`: starts an exe, or opens a URL in the default browser.
pub(crate) fn shell_execute(file: &OsStr, params: Option<&OsStr>) -> Result<()> {
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

    #[test]
    fn dashboard_url_is_the_loopback_page() {
        assert_eq!(dashboard_url(47800), "http://127.0.0.1:47800/");
    }

    /// Starts `cmd.exe /c exit 0` (it ends at once): the same call that starts `clarim window`, quoted path included.
    #[test]
    fn spawn_detached_starts_a_process_without_waiting_for_it() {
        let cmd = std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let pid = spawn_detached(&format!("\"{cmd}\" /c exit 0")).expect("cmd.exe starts");
        assert!(pid > 0);
        assert!(spawn_detached(r#""C:\no such dir\nothing.exe" window"#).is_err(), "a missing exe is an error, not a silent no-op");
    }

    /// Opens a real browser window, so it only runs on request:
    /// `cargo test -p clarim open_in_browser_launches -- --ignored`
    #[test]
    #[ignore]
    fn open_in_browser_launches() {
        open_in_browser(&dashboard_url(clarim_core::DEFAULT_PORT)).expect("open_in_browser");
    }
}
