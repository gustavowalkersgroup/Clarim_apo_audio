//! Windows service that keeps the per-user Clarim agent running.
//!
//! The service (LocalSystem, auto start) does no audio work. It launches `clarim.exe agent` into
//! the active console session and relaunches it when it dies (see `supervisor`). `install` copies
//! the exe to the Program Files folder first, so the service binary is not in a user-writable folder.
//!
//! Besides supervising, the service writes the Equalizer APO output-EQ file for the agent (`eqapo`): the
//! agent cannot write to Program Files, so it asks over a named pipe (`eqapo_send`).
//!
//! The same pipe also carries the Equalizer APO management commands (`apo_proto`): install the pinned official
//! installer when APO is missing (`apo_installer`), attach / detach APO on one playback device by editing the
//! audio device registry (`apo_plan` decides, `apo_reg` writes, `apo_service` sequences and validates), and
//! restart Windows Audio so the change takes effect (`audio_restart`). The agent calls `apo_status`,
//! `apo_install`, `apo_set_device` and `apo_restart_audio`; docs/eqapo.md has the registry mapping.

#[macro_use]
mod user_error;

mod apo_installer;
mod apo_plan;
mod apo_proto;
mod apo_reg;
mod apo_service;
mod audio_restart;
mod audiodg;
mod eqapo;
mod sha256;
mod supervisor;

pub use apo_proto::{apo_install, apo_restart_audio, apo_set_device, apo_status};
pub use apo_service::ApoServiceStatus;
pub use eqapo::{eqapo_send, eqapo_service_available};

/// The Equalizer APO version the service installs (the pinned installer), and its download size in MB
/// (SourceForge lists 12.0 MB), for the dashboard text.
pub const EQUALIZER_APO_VERSION: &str = "1.4.2";
pub const EQUALIZER_APO_DOWNLOAD_MB: u32 = 12;

use std::ffi::{c_void, OsString};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{GUID, PWSTR};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_LOCK_VIOLATION, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE,
    ERROR_SERVICE_NOT_ACTIVE, ERROR_SHARING_VIOLATION, ERROR_USER_MAPPED_FILE, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE,
};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_ProgramFiles, FOLDERID_System, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType, SessionChangeReason,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, Error as ServiceError};

use supervisor::Event;

pub const SERVICE_NAME: &str = "Clarim";
const DISPLAY_NAME: &str = "Clarim Audio";
const DESCRIPTION: &str = "Mantém o Clarim (otimizador de microfone e canais de saída de áudio) \
                           em execução na sessão do usuário.";

fn win32(e: &ServiceError, code: WIN32_ERROR) -> bool {
    matches!(e, ServiceError::Winapi(io) if io.raw_os_error() == i32::try_from(code.0).ok())
}

/// A Windows known folder, asked of the shell and not of an environment variable: the variables are
/// the caller's to change, and these folders decide where an elevated install writes and runs things.
fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` is a valid GUID; KF_FLAG_DEFAULT with a null token asks for the system-wide folder.
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()) }.ok()?;
    // SAFETY: on success `path` is a NUL-terminated UTF-16 string the shell allocated. It is copied
    // out first, then freed exactly once with CoTaskMemFree, as SHGetKnownFolderPath requires.
    let folder = unsafe {
        let folder = PathBuf::from(OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0.cast::<c_void>().cast_const()));
        folder
    };
    folder.is_absolute().then_some(folder) // never a path relative to the cwd
}

fn install_dir() -> PathBuf {
    known_folder(&FOLDERID_ProgramFiles).unwrap_or_else(|| PathBuf::from(r"C:\Program Files")).join("Clarim")
}

fn installed_exe() -> PathBuf {
    install_dir().join("clarim.exe")
}

/// `install` and `uninstall` append their log (and their final error) here: an elevated run started without a
/// console (a UAC prompt from the Start menu or PowerShell) has no stderr to show it on. Kept by `uninstall`.
const INSTALL_LOG: &str = "install.log";

pub fn install_log_path() -> PathBuf {
    install_dir().join(INSTALL_LOG)
}

/// The Start-menu entry for every user: `%ProgramData%\Microsoft\Windows\Start Menu\Programs\Clarim.lnk`.
fn shortcut_path() -> Option<PathBuf> {
    known_folder(&FOLDERID_CommonPrograms).map(|programs| programs.join("Clarim.lnk"))
}

/// PowerShell text that creates `lnk` pointing at `target`, with no arguments. Paths go in
/// single-quoted strings (taken verbatim, so the space in `C:\Program Files` is safe); a `'` inside
/// one is doubled.
fn shortcut_script(lnk: &Path, target: &Path) -> String {
    let quoted = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "''"));
    let dir = target.parent().unwrap_or(target);
    format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath = {}; \
         $s.WorkingDirectory = {}; $s.Description = 'Clarim'; $s.Save()",
        quoted(lnk),
        quoted(target),
        quoted(dir)
    )
}

/// Creates (or replaces) the Start-menu shortcut to the installed exe through one PowerShell call.
fn create_shortcut(target: &Path) -> Result<()> {
    write_shortcut(&shortcut_path().context("cannot locate the Start menu folder")?, target)
}

/// Writes `lnk` -> `target` through the system PowerShell (`shortcut_script`).
fn write_shortcut(lnk: &Path, target: &Path) -> Result<()> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Absolute path to the system PowerShell: an elevated process must not search the cwd or PATH for it.
    let powershell = known_folder(&FOLDERID_System)
        .context("cannot locate the System32 folder")?
        .join(r"WindowsPowerShell\v1.0\powershell.exe");
    let out = std::process::Command::new(&powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command", &shortcut_script(lnk, target)])
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("cannot run {}", powershell.display()))?;
    if !out.status.success() {
        bail!("PowerShell could not create {}: {}", lnk.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn remove_shortcut() {
    let Some(lnk) = shortcut_path() else { return };
    match std::fs::remove_file(&lnk) {
        Ok(()) => log::info!("removed {}", lnk.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("could not remove {}: {e}", lnk.display()),
    }
}

/// Connects with the rights install/uninstall need; being refused means the process is not elevated.
fn admin_manager() -> Result<ServiceManager> {
    let access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    ServiceManager::local_computer(None::<&str>, access).map_err(|e| {
        if win32(&e, ERROR_ACCESS_DENIED) {
            anyhow!("Execute como administrador: abra o terminal como administrador e repita o comando")
        } else {
            anyhow::Error::new(e).context("cannot open the service control manager")
        }
    })
}

/// `Ok(None)` when the service is not installed.
fn open_existing(manager: &ServiceManager, access: ServiceAccess) -> Result<Option<Service>> {
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => Ok(Some(service)),
        Err(e) if win32(&e, ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("cannot open the Clarim service")),
    }
}

/// How long `stop_and_wait` gives the service: well over its own stop path (`STOP_WAIT_HINT`, 12 s at most).
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

fn stop_and_wait(service: &Service) -> Result<()> {
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    match service.stop() {
        Ok(_) => {}
        // Already stopped, or already stopping: just wait below.
        Err(e) if win32(&e, ERROR_SERVICE_NOT_ACTIVE) || win32(&e, ERROR_SERVICE_CANNOT_ACCEPT_CTRL) => {}
        Err(e) => return Err(anyhow::Error::new(e).context("cannot stop the Clarim service")),
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while service.query_status()?.current_state != ServiceState::Stopped {
        if Instant::now() >= deadline {
            bail!("the Clarim service did not stop within {} s", STOP_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Starts the service; already running is fine.
fn start_service(service: &Service) -> Result<()> {
    match service.start::<&str>(&[]) {
        Ok(()) => Ok(()),
        Err(e) if win32(&e, ERROR_SERVICE_ALREADY_RUNNING) => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context("cannot start the Clarim service")),
    }
}

/// How long `start_again` waits for a stop that is still pending before it starts the service.
const START_AGAIN_WAIT: Duration = Duration::from_secs(15);

/// Brings the existing service back after a failed install (waiting for a stop that is still pending first),
/// so the old version keeps running.
fn start_again(service: &Service) -> Result<()> {
    let deadline = Instant::now() + START_AGAIN_WAIT;
    loop {
        match service.query_status()?.current_state {
            ServiceState::Running | ServiceState::StartPending => return Ok(()),
            ServiceState::Stopped => break,
            _ if Instant::now() >= deadline => bail!("the Clarim service is still stopping after {} s", START_AGAIN_WAIT.as_secs()),
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    start_service(service)
}

/// Runs `work`; when it fails, `restore` runs (best effort: its own failure is only logged) before the original
/// error is returned. `install` uses it so that a failure after the old service was stopped starts that service again.
fn restoring_on_error<T>(work: impl FnOnce() -> Result<T>, restore: impl FnOnce() -> Result<()>) -> Result<T> {
    work().inspect_err(|_| {
        log::warn!("the install failed; starting the existing Clarim service again (the error follows at the end)");
        match restore() {
            Ok(()) => log::info!("the existing Clarim service is running again"),
            Err(r) => log::error!("could not start the existing Clarim service again: {r:#}"),
        }
    })
}

/// Sharing violation, access denied, lock violation or a user-mapped section still open: another process (an
/// agent that was just killed, a virus scanner mapping the file) still holds the file for a moment.
fn is_busy(e: &std::io::Error) -> bool {
    [ERROR_SHARING_VIOLATION, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_USER_MAPPED_FILE]
        .iter()
        .any(|c| e.raw_os_error() == i32::try_from(c.0).ok())
}

const COPY_RETRY_STEP: Duration = Duration::from_millis(250);
const COPY_RETRY_FOR: Duration = Duration::from_secs(10);

/// Runs `op` again every `step` while it fails because the file is busy, for at most `limit`; any other
/// error, or the last busy one once the time is up, is returned as it is.
fn retry_while_busy<T>(mut op: impl FnMut() -> std::io::Result<T>, step: Duration, limit: Duration) -> std::io::Result<T> {
    let deadline = Instant::now() + limit;
    let mut retried = false;
    loop {
        match op() {
            Err(e) if is_busy(&e) && Instant::now() < deadline => {
                if !retried {
                    log::warn!("the file is in use ({e}); retrying for up to {} s", limit.as_secs());
                    retried = true;
                }
                std::thread::sleep(step);
            }
            other => return other,
        }
    }
}

/// Copies `from` to `to` through `to.new` in the same folder, then renames that over `to` (both steps retried while
/// the file is busy). A copy that fails half way (disk full, I/O error, a scanner) only damages the `.new` file, which
/// is removed again: the exe `to` that a failed install must restart stays whole.
fn copy_over(from: &Path, to: &Path) -> std::io::Result<()> {
    let staged = to.with_extension("exe.new");
    let done = retry_while_busy(|| std::fs::copy(from, &staged), COPY_RETRY_STEP, COPY_RETRY_FOR)
        .and_then(|_| retry_while_busy(|| std::fs::rename(&staged, to), COPY_RETRY_STEP, COPY_RETRY_FOR));
    if done.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    done
}

/// Ids of every process whose executable file name is `name` (case-insensitive). Empty when the process
/// list cannot be read.
pub(crate) fn pids_named(name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    // SAFETY: plain query call; the returned snapshot handle is owned (and closed) below.
    let snapshot_owner = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        // SAFETY: CreateToolhelp32Snapshot succeeded, so the handle is new and only we own it.
        Ok(h) => unsafe { OwnedHandle::from_raw_handle(h.0) },
        Err(e) => {
            log::warn!("cannot list processes: {e}");
            return pids;
        }
    };
    let snapshot = HANDLE(snapshot_owner.as_raw_handle());
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    // SAFETY: `entry.dwSize` is set and `snapshot` stays open for the whole loop because
    // `snapshot_owner` lives until the end of the function.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while more {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(name) {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: same as Process32FirstW above.
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    pids
}

/// Terminates every other process whose image is exactly `exe`, so the file can be replaced or
/// deleted. Best effort: failures are logged and skipped.
fn kill_processes_running(exe: &Path) {
    let Some(name) = exe.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    // SAFETY: trivial query of this process's id.
    let own_pid = unsafe { GetCurrentProcessId() };
    for pid in pids_named(&name).into_iter().filter(|&pid| pid != own_pid) {
        kill_if_image_is(pid, exe);
    }
}

fn kill_if_image_is(pid: u32, exe: &Path) {
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE;
    // SAFETY: plain open call; the handle is wrapped in an OwnedHandle right away.
    let Ok(process) = (unsafe { OpenProcess(access, false, pid) }) else {
        return; // gone already, or not ours to touch
    };
    // SAFETY: OpenProcess succeeded, so the handle is new and only we own it.
    let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
    let process_handle = HANDLE(process.as_raw_handle());

    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is valid for `len` UTF-16 units and `len` is a valid in/out pointer.
    let image = unsafe {
        QueryFullProcessImageNameW(process_handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    };
    if image.is_err() {
        return;
    }
    let image = OsString::from_wide(&buf[..len as usize]);
    if !image.eq_ignore_ascii_case(exe.as_os_str()) {
        return;
    }
    // SAFETY: the handle has terminate and synchronize access and stays open for both calls.
    unsafe {
        match TerminateProcess(process_handle, 1) {
            Ok(()) => log::info!("terminated stray process {pid} ({})", exe.display()),
            Err(e) => log::warn!("cannot terminate process {pid}: {e}"),
        }
        WaitForSingleObject(process_handle, 5000);
    }
}

fn service_info(exe: PathBuf) -> ServiceInfo {
    ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        // The crate quotes the path (and arguments) when it builds the command line:
        // "C:\Program Files\Clarim\clarim.exe" service
        executable_path: exe,
        launch_arguments: vec!["service".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    }
}

/// Restart after 5 s, three times; the failure count resets after a day without failures.
/// These only fire when the service process dies without reporting Stopped, not on a normal stop.
fn failure_actions() -> ServiceFailureActions {
    let restart = ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) };
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart.clone(), restart.clone(), restart]),
    }
}

/// Needs admin: copy the exe to Program Files, register the auto-start service, start it, and add the
/// Start-menu shortcut.
pub fn install() -> Result<()> {
    let manager = admin_manager()?;
    let current = std::env::current_exe().context("cannot locate the running executable")?;
    let old = open_existing(&manager, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START)?;
    let work = || replace_and_start(&manager, &current, old.as_ref());
    match &old {
        // From the first stop on, any failure must not leave the user without Clarim: the old service starts again.
        Some(service) => restoring_on_error(work, || start_again(service)),
        None => work(), // a first install has nothing to restore (and the log must not claim otherwise)
    }
}

/// The body of `install`: stop the old service, replace the exe, (re)configure and start the service.
fn replace_and_start(manager: &ServiceManager, current: &Path, old: Option<&Service>) -> Result<()> {
    let dest = installed_exe();
    log::info!("installing {} as {}", current.display(), dest.display());

    // The running service and its agent hold the installed exe open: stop both before replacing it.
    if let Some(service) = old {
        log::info!("stopping the running Clarim service (up to {} s)", STOP_TIMEOUT.as_secs());
        stop_and_wait(service)?;
        log::info!("the Clarim service stopped");
    }
    kill_processes_running(&dest);

    let already_there = matches!(
        (std::fs::canonicalize(current), std::fs::canonicalize(&dest)),
        (Ok(a), Ok(b)) if a == b
    );
    if !already_there {
        std::fs::create_dir_all(install_dir())
            .with_context(|| format!("cannot create {}", install_dir().display()))?;
        copy_over(current, &dest)
            .with_context(|| format!("cannot copy {} to {}", current.display(), dest.display()))?;
        log::info!("copied the executable to {}", dest.display());
    }

    let info = service_info(dest.clone());
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = match open_existing(manager, access)? {
        Some(service) => {
            service.change_config(&info).context("cannot update the Clarim service")?;
            service
        }
        None => manager.create_service(&info, access).context("cannot create the Clarim service")?,
    };
    // The service is what matters; a missing Start-menu entry only costs a click, so it must not fail the install.
    if let Err(e) = create_shortcut(&dest) {
        log::warn!("the Start menu shortcut was not created (the tray icon still opens the dashboard): {e:#}");
    }
    service.set_description(DESCRIPTION).context("cannot set the service description")?;
    service
        .update_failure_actions(failure_actions())
        .context("cannot set the service recovery actions")?;
    start_service(&service)?;
    log::info!("Clarim service installed and started ({})", dest.display());
    Ok(())
}

/// Stop and delete the service, remove the Start-menu shortcut, kill agents, remove the Program Files
/// dir (best effort).
pub fn uninstall() -> Result<()> {
    let manager = admin_manager()?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    match open_existing(&manager, access)? {
        Some(service) => {
            log::info!("stopping the Clarim service (up to {} s)", STOP_TIMEOUT.as_secs());
            stop_and_wait(&service)?;
            match service.delete() {
                Ok(()) => {}
                Err(e) if win32(&e, ERROR_SERVICE_MARKED_FOR_DELETE) => {}
                Err(e) => return Err(anyhow::Error::new(e).context("cannot delete the Clarim service")),
            }
            log::info!("Clarim service deleted");
        }
        None => log::info!("Clarim service was not installed"),
    }

    remove_shortcut();
    eqapo::remove();
    // Agents the service launched run from the installed copy; they would block removing it.
    kill_processes_running(&installed_exe());
    let dir = install_dir();
    if dir.exists() {
        match remove_install_dir(&dir) {
            Ok(()) => log::info!("removed {} (the {BACKUP_DIR} folder and {INSTALL_LOG}, if any, are kept)", dir.display()),
            // Typical when `clarim uninstall` itself runs from that folder: a running exe cannot be deleted.
            Err(e) => log::warn!("could not fully remove {}: {e}", dir.display()),
        }
    }
    Ok(())
}

/// The saved original effect settings of the devices Clarim attached Equalizer APO to
/// (`apo_reg::backup_device`). Uninstalling Clarim keeps them: they are the only copy of how those devices were.
const BACKUP_DIR: &str = "backup";

/// Deletes everything in `dir` except `BACKUP_DIR` and `INSTALL_LOG` (the uninstall's own record, still open),
/// then `dir` itself when nothing is left. Keeps going after a file that cannot be deleted (a running exe) and
/// returns the first such error.
fn remove_install_dir(dir: &Path) -> std::io::Result<()> {
    let mut first_error = None;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if [BACKUP_DIR, INSTALL_LOG].iter().any(|kept| entry.file_name().eq_ignore_ascii_case(kept)) {
            continue;
        }
        let path = entry.path();
        let removed = if entry.file_type()?.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        if let Err(e) = removed {
            first_error.get_or_insert(e);
        }
    }
    if let Some(e) = first_error {
        return Err(e);
    }
    if dir.join(BACKUP_DIR).is_dir() || dir.join(INSTALL_LOG).exists() {
        return Ok(()); // the backups and the log stay, so does their folder
    }
    std::fs::remove_dir(dir)
}

pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

define_windows_service!(ffi_service_main, service_main);

/// Entry for `clarim service` (service dispatcher; blocks until the service stops).
pub fn run_service() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        if win32(&e, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) {
            anyhow!("`clarim service` is started by the Windows Service Control Manager; use `clarim install`")
        } else {
            anyhow::Error::new(e).context("cannot start the service dispatcher")
        }
    })
}

/// Control codes the supervisor cares about; `None` for the rest.
fn control_event(control: &ServiceControl) -> Option<Event> {
    match control {
        ServiceControl::Stop | ServiceControl::Shutdown => Some(Event::Stop),
        ServiceControl::SessionChange(change) => match change.reason {
            SessionChangeReason::SessionLogon | SessionChangeReason::ConsoleConnect => Some(Event::Logon),
            SessionChangeReason::SessionLogoff => Some(Event::Logoff(change.notification.session_id)),
            _ => None,
        },
        _ => None,
    }
}

/// How long a running Equalizer APO operation (a Windows Audio restart, an installer giving up) gets to
/// end before the service reports Stopped.
const APO_STOP_WAIT: Duration = Duration::from_secs(8);

/// How long the SCM may wait for a stop: terminating the agent takes at most ~3 s, then `APO_STOP_WAIT`.
/// Stays well under the `STOP_TIMEOUT` that `stop_and_wait` (install, uninstall) gives the service.
const STOP_WAIT_HINT: Duration = Duration::from_secs(12);

fn status_for(state: ServiceState, exit_code: ServiceExitCode) -> ServiceStatus {
    let controls_accepted = if state == ServiceState::Running {
        ServiceControlAccept::STOP | ServiceControlAccept::SESSION_CHANGE | ServiceControlAccept::SHUTDOWN
    } else {
        ServiceControlAccept::empty()
    };
    // A pending state without a checkpoint and a wait hint makes the SCM treat the service as hung.
    let (checkpoint, wait_hint) =
        if state == ServiceState::StopPending { (1, STOP_WAIT_HINT) } else { (0, Duration::default()) };
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted,
        exit_code,
        checkpoint,
        wait_hint,
        process_id: None,
    }
}

fn report(handle: &ServiceStatusHandle, state: ServiceState, exit_code: ServiceExitCode) {
    if let Err(e) = handle.set_service_status(status_for(state, exit_code)) {
        log::warn!("cannot report service state {state:?}: {e}");
    }
}

fn service_main(_arguments: Vec<OsString>) {
    let (tx, rx) = mpsc::channel();
    let handler = move |control: ServiceControl| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::SessionChange(_) => {
            if let Some(event) = control_event(&control) {
                let _ = tx.send(event);
            }
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = match service_control_handler::register(SERVICE_NAME, handler) {
        Ok(status) => status,
        Err(e) => {
            log::error!("cannot register the service control handler: {e}");
            return;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            log::error!("cannot locate the service executable: {e}");
            report(&status, ServiceState::Stopped, ServiceExitCode::ServiceSpecific(1));
            return;
        }
    };
    report(&status, ServiceState::Running, ServiceExitCode::NO_ERROR);
    log::info!("service running, supervising {}", exe.display());
    // The output-EQ / Equalizer APO pipe is a bonus: without it the service still does its real job.
    let apo = apo_service::ApoService::new(apo_service::System);
    let handler = {
        let apo = Arc::clone(&apo);
        move |line: &[u8]| apo_proto::handle(&apo, &eqapo::apply_installed, line)
    };
    let eq_pipe = eqapo::Server::start(eqapo::PIPE_NAME, eqapo::PIPE_SDDL, handler)
        .map_err(|e| log::error!("the output EQ pipe was not started: {e:#}"))
        .ok();
    // StopPending goes out the moment the stop is seen, before the agent is terminated. An installer that
    // is running is told to give up at once (it kills what it started) and no new operation may begin.
    supervisor::run(exe, rx, || {
        apo.shutdown();
        report(&status, ServiceState::StopPending, ServiceExitCode::NO_ERROR);
    });
    // The process must not exit between the stop and the start of Windows Audio (no sound until reboot),
    // or while an installer still runs: give a running operation a bounded time to end.
    if !apo.wait_until_idle(APO_STOP_WAIT) {
        log::warn!("an Equalizer APO operation was still running {} s after the stop; exiting anyway", APO_STOP_WAIT.as_secs());
    }
    drop(eq_pipe);
    report(&status, ServiceState::Stopped, ServiceExitCode::NO_ERROR);
    log::info!("service stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_service::service::{SessionChangeParam, SessionNotification};

    fn session_change(reason: SessionChangeReason, session_id: u32) -> ServiceControl {
        ServiceControl::SessionChange(SessionChangeParam {
            reason,
            notification: SessionNotification { size: 8, session_id },
        })
    }

    #[test]
    fn service_name_is_stable() {
        assert_eq!(SERVICE_NAME, "Clarim");
    }

    #[test]
    fn maps_control_codes_to_supervisor_events() {
        assert_eq!(control_event(&ServiceControl::Stop), Some(Event::Stop));
        assert_eq!(control_event(&ServiceControl::Shutdown), Some(Event::Stop));
        assert_eq!(control_event(&session_change(SessionChangeReason::SessionLogon, 3)), Some(Event::Logon));
        assert_eq!(control_event(&session_change(SessionChangeReason::ConsoleConnect, 3)), Some(Event::Logon));
        assert_eq!(
            control_event(&session_change(SessionChangeReason::SessionLogoff, 3)),
            Some(Event::Logoff(3))
        );
        assert_eq!(control_event(&session_change(SessionChangeReason::SessionLock, 3)), None);
        assert_eq!(control_event(&ServiceControl::Interrogate), None);
    }

    #[test]
    fn recovery_is_three_restarts_after_five_seconds() {
        let actions = failure_actions().actions.unwrap();
        assert_eq!(actions.len(), 3);
        assert!(actions
            .iter()
            .all(|a| a.action_type == ServiceActionType::Restart && a.delay == Duration::from_secs(5)));
    }

    #[test]
    fn run_service_outside_the_scm_explains_itself() {
        // StartServiceCtrlDispatcherW fails at once when the process was not started by the SCM.
        let err = run_service().expect_err("a test process is not a service");
        assert!(err.to_string().contains("Service Control Manager"), "{err:#}");
    }

    #[test]
    fn admin_check_reports_portuguese_hint_when_not_elevated() {
        // Connects only; touches nothing. Elevated runs get a manager, others get the hint.
        if let Err(e) = admin_manager() {
            assert!(e.to_string().starts_with("Execute como administrador"), "{e:#}");
        }
    }

    #[test]
    fn kills_only_processes_running_the_given_image() {
        let Some(system_root) = std::env::var_os("SystemRoot") else { return };
        let ping = Path::new(&system_root).join(r"System32\PING.EXE");
        if !ping.is_file() {
            return;
        }
        // A private copy of a harmless exe gives a process image that is unique to this test.
        let dir = std::env::temp_dir().join(format!("clarim-svc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let victim_exe = dir.join("clarim-svc-victim.exe");
        std::fs::copy(&ping, &victim_exe).unwrap();
        let mut victim = std::process::Command::new(&victim_exe)
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();

        kill_processes_running(&victim_exe);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while Instant::now() < deadline && !exited {
            exited = victim.try_wait().unwrap().is_some();
            std::thread::sleep(Duration::from_millis(50));
        }
        if !exited {
            let _ = victim.kill();
        }
        let _ = victim.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(exited, "the process running the given image was not terminated");
    }

    #[test]
    fn uninstall_removes_the_install_folder_but_keeps_the_device_backups() {
        let root = std::env::temp_dir().join(format!("clarim-svc-uninstall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::create_dir_all(root.join(BACKUP_DIR)).unwrap();
        std::fs::write(root.join("clarim.exe"), b"x").unwrap();
        std::fs::write(root.join("cache").join("installer.exe"), b"x").unwrap();
        std::fs::write(root.join(BACKUP_DIR).join("{guid}.json"), b"{}").unwrap();
        std::fs::write(root.join(INSTALL_LOG), b"uninstall record").unwrap();
        remove_install_dir(&root).expect("removes what it can");
        assert!(!root.join("clarim.exe").exists() && !root.join("cache").exists());
        assert!(root.join(BACKUP_DIR).join("{guid}.json").is_file(), "the saved original settings stay");
        assert!(root.join(INSTALL_LOG).is_file(), "the uninstall's own record stays");
        // the log alone keeps the folder; with neither backups nor log it goes too
        std::fs::remove_dir_all(root.join(BACKUP_DIR)).unwrap();
        remove_install_dir(&root).expect("only the log is left");
        assert!(root.join(INSTALL_LOG).is_file());
        std::fs::remove_file(root.join(INSTALL_LOG)).unwrap();
        remove_install_dir(&root).expect("empty folder removed");
        assert!(!root.exists());
    }

    fn io_err(code: WIN32_ERROR) -> std::io::Error {
        std::io::Error::from_raw_os_error(code.0 as i32)
    }

    #[test]
    fn only_a_busy_file_is_worth_a_retry() {
        for busy in [ERROR_SHARING_VIOLATION, ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_USER_MAPPED_FILE] {
            assert!(is_busy(&io_err(busy)), "{busy:?}");
        }
        assert!(!is_busy(&std::io::Error::from(std::io::ErrorKind::NotFound)));
        assert!(!is_busy(&io_err(windows::Win32::Foundation::ERROR_FILE_NOT_FOUND)));
        assert!(!is_busy(&io_err(windows::Win32::Foundation::ERROR_DISK_FULL)));
    }

    #[test]
    fn a_busy_copy_is_retried_until_it_works_or_the_time_is_up() {
        let (step, limit) = (Duration::from_millis(1), Duration::from_secs(5));
        // busy twice, then fine: three tries
        let mut calls = 0;
        let done = retry_while_busy(
            || {
                calls += 1;
                if calls < 3 { Err(io_err(ERROR_SHARING_VIOLATION)) } else { Ok(calls) }
            },
            step,
            limit,
        );
        assert_eq!(done.unwrap(), 3);
        // any other error is final at once
        let mut calls = 0;
        let err = retry_while_busy(
            || {
                calls += 1;
                Err::<(), _>(io_err(windows::Win32::Foundation::ERROR_DISK_FULL))
            },
            step,
            limit,
        )
        .unwrap_err();
        assert_eq!((calls, is_busy(&err)), (1, false));
        // busy for good: tried more than once, then the busy error comes back
        let mut calls = 0;
        let err = retry_while_busy(
            || {
                calls += 1;
                Err::<(), _>(io_err(ERROR_ACCESS_DENIED))
            },
            step,
            Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(calls > 1 && is_busy(&err), "{calls} tries, {err}");
    }

    #[test]
    fn the_exe_is_replaced_through_a_staged_copy_that_a_failure_never_leaves_behind() {
        let dir = std::env::temp_dir().join(format!("clarim-svc-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (new, installed, staged) = (dir.join("new.exe"), dir.join("clarim.exe"), dir.join("clarim.exe.new"));
        std::fs::write(&installed, b"old").unwrap();
        // a source that cannot be copied: the installed exe stays whole, no staged file is left
        assert!(copy_over(&new, &installed).is_err());
        assert_eq!(std::fs::read(&installed).unwrap(), b"old");
        assert!(!staged.exists());
        // a leftover staged file from an earlier run is just overwritten; the copy replaces the installed exe
        std::fs::write(&staged, b"leftover").unwrap();
        std::fs::write(&new, b"new").unwrap();
        copy_over(&new, &installed).expect("copies over the old exe");
        assert_eq!(std::fs::read(&installed).unwrap(), b"new");
        assert!(!staged.exists() && new.is_file());
        // first install: nothing to replace
        std::fs::remove_file(&installed).unwrap();
        copy_over(&new, &installed).expect("copies to a new name");
        assert_eq!(std::fs::read(&installed).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_install_starts_the_old_service_again_and_keeps_the_original_error() {
        use std::cell::RefCell;
        let order = RefCell::new(Vec::new());
        // fine: nothing to restore
        let ok = restoring_on_error(
            || {
                order.borrow_mut().push("work");
                Ok(7)
            },
            || {
                order.borrow_mut().push("restore");
                Ok(())
            },
        );
        assert_eq!((ok.unwrap(), order.borrow().clone()), (7, vec!["work"]));
        // failed: restore runs once, after the work, and the work's error is what comes back
        order.borrow_mut().clear();
        let err = restoring_on_error(
            || -> Result<()> {
                order.borrow_mut().push("work");
                Err(anyhow!("disk full").context("cannot copy"))
            },
            || {
                order.borrow_mut().push("restore");
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(format!("{err:#}"), "cannot copy: disk full");
        assert_eq!(*order.borrow(), ["work", "restore"]);
        // a restore that fails too does not replace the error
        let err = restoring_on_error(|| -> Result<()> { bail!("copy failed") }, || bail!("cannot start")).unwrap_err();
        assert_eq!(err.to_string(), "copy failed");
    }

    #[test]
    fn install_dir_is_under_program_files() {
        let exe = installed_exe();
        assert!(exe.is_absolute());
        assert!(exe.ends_with(r"Clarim\clarim.exe"));
        assert!(install_log_path().ends_with(r"Clarim\install.log"));
        // resolved by the shell, so it is the real Program Files even when the variable is changed
        let program_files = known_folder(&FOLDERID_ProgramFiles).expect("the shell knows Program Files");
        assert_eq!(install_dir(), program_files.join("Clarim"));
    }

    #[test]
    fn stop_pending_carries_a_checkpoint_and_a_wait_hint() {
        let stopping = status_for(ServiceState::StopPending, ServiceExitCode::NO_ERROR);
        assert_eq!((stopping.checkpoint, stopping.wait_hint), (1, STOP_WAIT_HINT));
        // agent termination (~3 s) + the wait for a running APO operation fits the hint, and the hint fits
        // twice into what `stop_and_wait` gives the service
        assert!(Duration::from_secs(3) + APO_STOP_WAIT <= STOP_WAIT_HINT && STOP_WAIT_HINT * 2 <= STOP_TIMEOUT);
        assert!(stopping.controls_accepted.is_empty(), "no more controls while stopping");
        let running = status_for(ServiceState::Running, ServiceExitCode::NO_ERROR);
        assert_eq!((running.checkpoint, running.wait_hint), (0, Duration::ZERO));
        assert!(running.controls_accepted.contains(ServiceControlAccept::STOP));
        let stopped = status_for(ServiceState::Stopped, ServiceExitCode::NO_ERROR);
        assert_eq!((stopped.checkpoint, stopped.wait_hint), (0, Duration::ZERO));
    }

    /// Runs the real PowerShell command, but writes the .lnk in a temp folder (a path with a space and an
    /// apostrophe), never in the Start menu: proves the quoting survives the trip through the command line.
    #[test]
    fn powershell_writes_the_shortcut_with_awkward_paths() {
        let dir = std::env::temp_dir().join(format!("clarim svc lnk's {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lnk = dir.join("Clarim.lnk");
        let result = write_shortcut(&lnk, Path::new(r"C:\Program Files\Clarim\clarim.exe"));
        let bytes = std::fs::read(&lnk).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        result.expect("PowerShell creates the shortcut");
        assert!(bytes.len() > 76, "a shell link file has a 76-byte header at least");
        assert_eq!(&bytes[..4], &[0x4C, 0, 0, 0], "shell link header size");
        let text: Vec<u8> = bytes.iter().copied().filter(|b| *b != 0).collect(); // UTF-16 -> ASCII-ish
        assert!(String::from_utf8_lossy(&text).contains("clarim.exe"), "the link must name the target exe");
    }

    #[test]
    fn start_menu_shortcut_points_at_the_installed_exe_without_arguments() {
        let lnk = shortcut_path().expect("the shell knows the common Start menu");
        assert!(lnk.ends_with(r"Start Menu\Programs\Clarim.lnk"), "{}", lnk.display());

        let script = shortcut_script(
            Path::new(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Clarim.lnk"),
            Path::new(r"C:\Program Files\Clarim\clarim.exe"),
        );
        assert!(script.contains(r"CreateShortcut('C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Clarim.lnk')"), "{script}");
        assert!(script.contains(r"$s.TargetPath = 'C:\Program Files\Clarim\clarim.exe';"), "{script}");
        assert!(script.contains(r"$s.WorkingDirectory = 'C:\Program Files\Clarim';"), "{script}");
        assert!(!script.contains("Arguments"), "the shortcut must start clarim.exe with no arguments: {script}");
        // a quote in a path cannot break out of the PowerShell string
        let tricky = shortcut_script(Path::new(r"C:\a'b.lnk"), Path::new(r"C:\x\y.exe"));
        assert!(tricky.contains(r"CreateShortcut('C:\a''b.lnk')"), "{tricky}");
    }
}
