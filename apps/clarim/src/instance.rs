//! Single-instance guards: named mutexes in the user's session namespace, so each logged-on user gets at
//! most one agent (`Local\ClarimAgent`) and at most one dashboard window (`Local\ClarimWindow`). The guided install
//! (`clarim setup`) has two: its launcher per user session and its elevated copy per PC.

use std::os::windows::io::{FromRawHandle, OwnedHandle};

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

/// Holds a mutex; the handle closes (releasing the name) on drop.
pub struct InstanceGuard {
    _mutex: OwnedHandle,
}

/// Claims the agent slot. `Ok(None)` means another agent already holds it. A mutex that cannot be
/// created at all is an error, not "already running": the caller must not exit as if an agent existed.
pub fn acquire() -> Result<Option<InstanceGuard>> {
    acquire_named(w!("Local\\ClarimAgent"))
}

/// Claims the dashboard window slot (`clarim window`); `Ok(None)` means a window process already holds it.
pub fn acquire_window() -> Result<Option<InstanceGuard>> {
    acquire_named(w!("Local\\ClarimWindow"))
}

/// Claims the slot of the guided install's launcher: the user's own, not elevated process that asks for administrator rights
/// and waits for the elevated copy. A different name from `acquire_setup` on purpose: the launcher holds this one while
/// the elevated copy it started takes the other.
pub fn acquire_setup_launcher() -> Result<Option<InstanceGuard>> {
    acquire_named(w!("Local\\ClarimSetupLauncher"))
}

/// Claims the slot of the elevated guided install. `Global\`: the install changes the whole PC (Program Files, the service),
/// so one at a time, whoever started it.
pub fn acquire_setup() -> Result<Option<InstanceGuard>> {
    acquire_named(w!("Global\\ClarimSetup"))
}

fn acquire_named(name: PCWSTR) -> Result<Option<InstanceGuard>> {
    // SAFETY: `name` is a static NUL-terminated string and no security attributes are passed.
    let handle = unsafe { CreateMutexW(None, false, name) }
        .map_err(|e| anyhow!("cannot create the single-instance mutex: {e}"))?;
    // SAFETY: CreateMutexW succeeded, so the handle is valid and only we own this reference to it.
    // Owning it first means the already-exists path below closes it too.
    let mutex = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    // SAFETY: reads this thread's last error, set by the CreateMutexW call just above (nothing ran in
    // between): CreateMutexW returns a handle to the existing mutex and sets ERROR_ALREADY_EXISTS.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    Ok((!existed).then_some(InstanceGuard { _mutex: mutex }))
}

/// True when an agent (in this user session) holds the mutex. Read-only probe.
pub fn agent_running() -> bool {
    // SAFETY: the name is a static NUL-terminated string.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("Local\\ClarimAgent")) } {
        Ok(handle) => {
            // SAFETY: OpenMutexW succeeded, so the handle is valid; wrapping it closes it on drop.
            drop(unsafe { OwnedHandle::from_raw_handle(handle.0) });
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test on purpose: the mutex name is process-wide, so parallel tests would race on it.
    #[test]
    fn guard_is_exclusive_and_visible_to_the_probe() {
        assert!(!agent_running(), "an agent is running on this machine; stop it to run this test");
        let first = acquire().expect("mutex is creatable").expect("first acquire");
        assert!(agent_running());
        assert!(acquire().expect("mutex is creatable").is_none(), "second acquire must fail while the first guard lives");
        drop(first);
        assert!(!agent_running());
        assert!(acquire().expect("mutex is creatable").is_some(), "the name is free again after the guard dropped");
    }

    /// Separate from the agent test: these two names are used by nothing else, so it cannot race with it.
    #[test]
    fn the_setup_launcher_and_the_elevated_setup_each_have_their_own_exclusive_slot() {
        let launcher = acquire_setup_launcher().expect("mutex is creatable").expect("first launcher");
        let setup = acquire_setup().expect("mutex is creatable").expect("the launcher's slot does not block the elevated copy");
        assert!(acquire_setup_launcher().expect("mutex is creatable").is_none(), "a second launcher is refused");
        assert!(acquire_setup().expect("mutex is creatable").is_none(), "a second elevated setup is refused");
        drop((launcher, setup));
        assert!(acquire_setup_launcher().expect("mutex is creatable").is_some(), "free again after the guards dropped");
        assert!(acquire_setup().expect("mutex is creatable").is_some());
    }
}
