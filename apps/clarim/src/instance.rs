//! Single-instance guard for the agent: a named mutex in the user's session namespace
//! (`Local\ClarimAgent`), so each logged-on user gets at most one agent.

use std::os::windows::io::{FromRawHandle, OwnedHandle};

use anyhow::{anyhow, Result};
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

/// Holds the agent mutex; the handle closes (releasing the name) on drop.
pub struct InstanceGuard {
    _mutex: OwnedHandle,
}

/// Claims the agent slot. `Ok(None)` means another agent already holds it. A mutex that cannot be
/// created at all is an error, not "already running": the caller must not exit as if an agent existed.
pub fn acquire() -> Result<Option<InstanceGuard>> {
    // SAFETY: the name is a static NUL-terminated string and no security attributes are passed.
    let handle = unsafe { CreateMutexW(None, false, w!("Local\\ClarimAgent")) }
        .map_err(|e| anyhow!("cannot create the agent mutex: {e}"))?;
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
}
