//! "Reiniciar audio": restart the Windows Audio service (`Audiosrv`) and the services that depend on it,
//! the way Equalizer APO's own `ServiceHelper::restartService` does: stop the running dependents, stop
//! `Audiosrv`, start it, start the dependents again in reverse. A new `audiodg.exe` then builds the audio
//! graph with the effect APOs the registry names now.
//!
//! `restart_audio` is pure orchestration over the `Services` trait (tested with a fake); `Scm` is the real
//! thing, a thin wrapper over the service control manager.

use std::mem::size_of;
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, EnumDependentServicesW, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
    StartServiceW, ENUM_SERVICE_STATUSW, SC_HANDLE, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_ACTIVE,
    SERVICE_CONTROL_STOP, SERVICE_ENUMERATE_DEPENDENTS, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED,
};

pub(crate) const AUDIO_SERVICE: &str = "Audiosrv";
const DEPENDENT_TIMEOUT: Duration = Duration::from_secs(15);
const AUDIO_TIMEOUT: Duration = Duration::from_secs(25);
/// Windows Audio was restarted this recently: another restart is refused (any local user may ask).
pub(crate) const MIN_GAP: Duration = Duration::from_secs(10);

pub(crate) const MSG_TOO_SOON: &str = "O áudio do Windows acabou de ser reiniciado. Espere alguns segundos.";

/// May a restart start now, given when the last one ended?
pub(crate) fn restart_allowed(last_end: Option<Instant>, now: Instant) -> bool {
    last_end.is_none_or(|t| now.saturating_duration_since(t) >= MIN_GAP)
}

/// The service control manager, as far as a restart needs it.
pub(crate) trait Services {
    /// The running services that depend on `service` (all levels), in the order to stop them.
    fn running_dependents(&self, service: &str) -> Result<Vec<String>>;
    /// Returns once the service is stopped (already stopped is fine).
    fn stop(&self, service: &str, timeout: Duration) -> Result<()>;
    /// Returns once the service is running (already running is fine). A start that is refused or falls
    /// back to stopped is tried again about every 2 s until `timeout`.
    fn start(&self, service: &str, timeout: Duration) -> Result<()>;
}

/// Restarts Windows Audio. `Ok` lists the dependents that did not come back (the audio itself did);
/// `Err` carries a PT-BR text that tells the user to reboot when a service refused to stop or start.
/// A failure before the audio service was stopped puts back the dependents already stopped; so does an
/// `Audiosrv` that will not start again (its start is retried inside `Services::start`).
pub(crate) fn restart_audio(svc: &dyn Services, mut progress: impl FnMut(&str)) -> Result<Vec<String>> {
    let dependents = svc.running_dependents(AUDIO_SERVICE).context("cannot list the services that depend on Windows Audio")?;
    log::info!("restarting Windows Audio; running dependents: {dependents:?}");

    progress("Parando os serviços de áudio...");
    // The order the SCM gives usually works; a second pass covers one that had to wait for another.
    let mut stopped: Vec<String> = Vec::new();
    let mut pending = dependents;
    let mut refused: Option<(String, anyhow::Error)> = None;
    for _pass in 0..2 {
        let mut next = Vec::new();
        refused = None;
        for name in pending {
            match svc.stop(&name, DEPENDENT_TIMEOUT) {
                Ok(()) => stopped.push(name),
                Err(e) => {
                    refused = Some((name.clone(), e));
                    next.push(name);
                }
            }
        }
        pending = next;
        if pending.is_empty() {
            break;
        }
    }
    let start_back = |names: &[String], svc: &dyn Services| -> Vec<String> {
        let mut failed = Vec::new();
        for name in names.iter().rev() {
            if let Err(e) = svc.start(name, DEPENDENT_TIMEOUT) {
                log::warn!("service {name} did not come back: {e:#}");
                failed.push(name.clone());
            }
        }
        failed
    };
    if let Some((name, e)) = refused.filter(|_| !pending.is_empty()) {
        log::warn!("cannot stop {name}: {e:#}");
        start_back(&stopped, svc);
        user_bail!("Não foi possível parar o serviço \"{name}\", que depende do áudio do Windows. Reinicie o computador.");
    }

    if let Err(e) = svc.stop(AUDIO_SERVICE, AUDIO_TIMEOUT) {
        log::warn!("cannot stop Windows Audio: {e:#}");
        start_back(&stopped, svc);
        user_bail!("O Windows Audio não parou a tempo. Reinicie o computador.");
    }
    progress("Iniciando o áudio do Windows...");
    if let Err(e) = svc.start(AUDIO_SERVICE, AUDIO_TIMEOUT) {
        log::error!("Windows Audio did not start again: {e:#}");
        // The dependents still go back up: starting one makes the SCM start Audiosrv first (one more
        // attempt for free) and the services that were running are not left down.
        start_back(&stopped, svc);
        user_bail!("O Windows Audio não voltou a iniciar. Reinicie o computador.");
    }
    progress("Religando os serviços que dependem do áudio...");
    Ok(start_back(&stopped, svc))
}

// ---------------------------------------------------------------------------------------------
// The real service control manager
// ---------------------------------------------------------------------------------------------

pub(crate) struct Scm;

/// A service (or manager) handle, closed on drop.
struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenSCManagerW / OpenServiceW and is closed exactly once.
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn open(name: &str, access: u32) -> Result<Handle> {
    // SAFETY: null machine and database names mean the local active database.
    let manager = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
        .map_err(|e| anyhow!("cannot open the service control manager: {e}"))?;
    let manager = Handle(manager);
    let name_w = wide(name);
    // SAFETY: `manager` is open and `name_w` is NUL-terminated and outlives the call.
    let service = unsafe { OpenServiceW(manager.0, PCWSTR(name_w.as_ptr()), access) }
        .map_err(|e| anyhow!("cannot open service {name}: {e}"))?;
    Ok(Handle(service))
}

/// `dwCurrentState` of the service.
fn current_state(service: &Handle) -> Result<u32> {
    let mut buf = [0u8; size_of::<SERVICE_STATUS_PROCESS>()];
    let mut needed = 0u32;
    // SAFETY: `buf` is exactly one SERVICE_STATUS_PROCESS long and `needed` is a valid out pointer.
    unsafe { QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(&mut buf), &mut needed) }
        .map_err(|e| anyhow!("cannot query the service state: {e}"))?;
    // SAFETY: the call filled `buf` with a SERVICE_STATUS_PROCESS (a plain-data struct); the read is
    // unaligned because a byte array has no alignment guarantee.
    Ok(unsafe { ptr::read_unaligned(buf.as_ptr().cast::<SERVICE_STATUS_PROCESS>()) }.dwCurrentState.0)
}

fn wait_for_state(service: &Handle, wanted: u32, timeout: Duration, what: &str) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if current_state(service)? == wanted {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("{what} did not finish within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Services for Scm {
    fn running_dependents(&self, service: &str) -> Result<Vec<String>> {
        let handle = open(service, SERVICE_ENUMERATE_DEPENDENTS)?;
        let (mut needed, mut count) = (0u32, 0u32);
        // First call: no buffer, so it only reports the size (ERROR_MORE_DATA, or success with none).
        // SAFETY: a null buffer with size 0 is the documented way to ask; both out pointers are valid.
        let first = unsafe { EnumDependentServicesW(handle.0, SERVICE_ACTIVE, None, 0, &mut needed, &mut count) };
        if first.is_ok() || needed == 0 {
            return Ok(Vec::new());
        }
        // u64 elements: the entries hold pointers, so the buffer must be 8-byte aligned.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
        // SAFETY: `buf` is at least `needed` bytes; the out pointers are valid.
        unsafe {
            EnumDependentServicesW(
                handle.0,
                SERVICE_ACTIVE,
                Some(buf.as_mut_ptr().cast::<ENUM_SERVICE_STATUSW>()),
                (buf.len() * 8) as u32,
                &mut needed,
                &mut count,
            )
        }
        .map_err(|e| anyhow!("cannot list the dependent services: {e}"))?;
        let entries = buf.as_ptr().cast::<ENUM_SERVICE_STATUSW>();
        let mut names = Vec::new();
        for i in 0..count as usize {
            // SAFETY: the call wrote `count` entries at the start of `buf`; each `lpServiceName` points to a
            // NUL-terminated string stored in the same buffer, which is still alive.
            let name = unsafe { (*entries.add(i)).lpServiceName.to_string() }.map_err(|e| anyhow!("bad service name: {e}"))?;
            names.push(name);
        }
        Ok(names)
    }

    fn stop(&self, service: &str, timeout: Duration) -> Result<()> {
        let handle = open(service, SERVICE_STOP | SERVICE_QUERY_STATUS)?;
        if current_state(&handle)? == SERVICE_STOPPED.0 {
            return Ok(());
        }
        let mut status = SERVICE_STATUS::default();
        // SAFETY: `handle` has SERVICE_STOP access and `status` is a valid out pointer.
        match unsafe { ControlService(handle.0, SERVICE_CONTROL_STOP, &mut status) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => return Ok(()),
            Err(e) => return Err(anyhow!("cannot stop {service}: {e}")),
        }
        wait_for_state(&handle, SERVICE_STOPPED.0, timeout, &format!("stopping {service}"))
    }

    fn start(&self, service: &str, timeout: Duration) -> Result<()> {
        let handle = open(service, SERVICE_START | SERVICE_QUERY_STATUS)?;
        let deadline = Instant::now() + timeout;
        let mut last_try: Option<Instant> = None;
        let mut refused: Option<anyhow::Error> = None;
        loop {
            match start_step(current_state(&handle)?, last_try.map(|t| t.elapsed())) {
                StartStep::Done => return Ok(()),
                StartStep::Try => {
                    last_try = Some(Instant::now());
                    // SAFETY: `handle` has SERVICE_START access; no arguments are passed.
                    match unsafe { StartServiceW(handle.0, None) } {
                        Ok(()) => refused = None,
                        Err(e) if e.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => refused = None,
                        Err(e) => refused = Some(anyhow!("cannot start {service}: {e}")),
                    }
                }
                StartStep::Wait => {}
            }
            if Instant::now() >= deadline {
                return Err(refused.unwrap_or_else(|| anyhow!("starting {service} did not finish within {timeout:?}")));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// How often `StartServiceW` is repeated while the service sits in STOPPED.
const START_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq)]
enum StartStep {
    Done,
    Try,
    Wait,
}

/// What `Scm::start` does next, given the service's `dwCurrentState` and the time since the last
/// `StartServiceW` (`None` = not called yet). A start that was accepted and then fell back to STOPPED (or
/// was refused) is tried again, as Equalizer APO's own `ServiceHelper` does ("sometimes, the service won't
/// start on the first try"); START_PENDING and STOP_PENDING are only waited out, so the SCM is not hammered.
fn start_step(state: u32, since_last_try: Option<Duration>) -> StartStep {
    if state == SERVICE_RUNNING.0 {
        StartStep::Done
    } else if state == SERVICE_STOPPED.0 && since_last_try.is_none_or(|t| t >= START_RETRY) {
        StartStep::Try
    } else {
        StartStep::Wait
    }
}


#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Records every call; `fail_stop` / `fail_start` name services that refuse (stop refusals can be
    /// limited to the first N attempts).
    #[derive(Default)]
    struct Fake {
        dependents: Vec<String>,
        calls: RefCell<Vec<String>>,
        fail_stop: Vec<(String, u32)>,
        fail_start: Vec<String>,
        stop_attempts: RefCell<Vec<String>>,
    }

    impl Services for Fake {
        fn running_dependents(&self, service: &str) -> Result<Vec<String>> {
            assert_eq!(service, AUDIO_SERVICE);
            Ok(self.dependents.clone())
        }
        fn stop(&self, service: &str, _t: Duration) -> Result<()> {
            let attempt = self.stop_attempts.borrow().iter().filter(|s| *s == service).count() as u32;
            self.stop_attempts.borrow_mut().push(service.to_string());
            if let Some((_, times)) = self.fail_stop.iter().find(|(n, _)| n == service) {
                if attempt < *times {
                    bail!("stuck");
                }
            }
            self.calls.borrow_mut().push(format!("stop {service}"));
            Ok(())
        }
        fn start(&self, service: &str, _t: Duration) -> Result<()> {
            if self.fail_start.iter().any(|n| n == service) {
                bail!("will not start");
            }
            self.calls.borrow_mut().push(format!("start {service}"));
            Ok(())
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dependents_stop_first_audio_cycles_and_dependents_come_back_in_reverse() {
        let fake = Fake { dependents: names(&["A", "B"]), ..Fake::default() };
        let mut said = Vec::new();
        let not_back = restart_audio(&fake, |m| said.push(m.to_string())).unwrap();
        assert!(not_back.is_empty());
        assert_eq!(*fake.calls.borrow(), ["stop A", "stop B", "stop Audiosrv", "start Audiosrv", "start B", "start A"]);
        assert_eq!(said.len(), 3, "one progress line per phase: {said:?}");
    }

    #[test]
    fn no_dependents_is_just_stop_and_start() {
        let fake = Fake::default();
        restart_audio(&fake, |_| {}).unwrap();
        assert_eq!(*fake.calls.borrow(), ["stop Audiosrv", "start Audiosrv"]);
    }

    #[test]
    fn a_dependent_that_has_to_wait_for_another_is_retried_once() {
        let fake = Fake { dependents: names(&["A", "B"]), fail_stop: vec![("A".into(), 1)], ..Fake::default() };
        restart_audio(&fake, |_| {}).unwrap();
        assert_eq!(*fake.calls.borrow(), ["stop B", "stop A", "stop Audiosrv", "start Audiosrv", "start A", "start B"]);
    }

    #[test]
    fn a_dependent_that_will_not_stop_aborts_and_puts_the_others_back() {
        let fake = Fake { dependents: names(&["A", "B"]), fail_stop: vec![("B".into(), 99)], ..Fake::default() };
        let err = restart_audio(&fake, |_| {}).unwrap_err().to_string();
        assert!(err.contains("\"B\"") && err.contains("Reinicie o computador"), "{err}");
        assert_eq!(*fake.calls.borrow(), ["stop A", "start A"], "Windows Audio itself was never touched");
    }

    #[test]
    fn audio_that_will_not_stop_or_start_says_to_reboot() {
        let fake = Fake { dependents: names(&["A"]), fail_stop: vec![("Audiosrv".into(), 99)], ..Fake::default() };
        let err = restart_audio(&fake, |_| {}).unwrap_err().to_string();
        assert!(err.contains("não parou") && err.contains("Reinicie o computador"), "{err}");
        assert_eq!(*fake.calls.borrow(), ["stop A", "start A"], "the dependent is restarted: audio never went down");

        let fake = Fake { dependents: names(&["A"]), fail_start: vec!["Audiosrv".into()], ..Fake::default() };
        let err = restart_audio(&fake, |_| {}).unwrap_err().to_string();
        assert!(err.contains("não voltou") && err.contains("Reinicie o computador"), "{err}");
        assert_eq!(*fake.calls.borrow(), ["stop A", "stop Audiosrv", "start A"], "the dependent is not left down");
    }

    #[test]
    fn a_start_is_retried_from_stopped_and_only_waited_out_while_pending() {
        use windows::Win32::System::Services::{SERVICE_START_PENDING, SERVICE_STOP_PENDING};
        let (running, stopped) = (SERVICE_RUNNING.0, SERVICE_STOPPED.0);
        let secs = Duration::from_secs;
        assert_eq!(start_step(running, None), StartStep::Done);
        assert_eq!(start_step(running, Some(secs(0))), StartStep::Done);
        assert_eq!(start_step(stopped, None), StartStep::Try, "first call");
        assert_eq!(start_step(stopped, Some(Duration::from_millis(400))), StartStep::Wait, "not again right away");
        assert_eq!(start_step(stopped, Some(START_RETRY)), StartStep::Try, "back in STOPPED after a start: try again");
        for pending in [SERVICE_START_PENDING.0, SERVICE_STOP_PENDING.0] {
            assert_eq!(start_step(pending, None), StartStep::Wait);
            assert_eq!(start_step(pending, Some(secs(60))), StartStep::Wait, "a pending state is never hammered");
        }
    }

    #[test]
    fn a_dependent_that_does_not_come_back_is_reported_not_fatal() {
        let fake = Fake { dependents: names(&["A", "B"]), fail_start: vec!["B".into()], ..Fake::default() };
        assert_eq!(restart_audio(&fake, |_| {}).unwrap(), ["B"]);
        assert!(fake.calls.borrow().contains(&"start A".to_string()), "the others still start");
    }

    #[test]
    fn restarts_are_rate_limited() {
        let t0 = Instant::now();
        assert!(restart_allowed(None, t0));
        assert!(!restart_allowed(Some(t0), t0 + Duration::from_secs(3)));
        assert!(restart_allowed(Some(t0), t0 + MIN_GAP));
    }

    /// Read-only: lists what depends on Windows Audio here (nothing is stopped).
    /// `cargo test -p clarim-svc live_dependents -- --ignored --nocapture`
    #[test]
    #[ignore = "queries the real service control manager"]
    fn live_dependents() {
        println!("{:?}", Scm.running_dependents(AUDIO_SERVICE));
    }
}
