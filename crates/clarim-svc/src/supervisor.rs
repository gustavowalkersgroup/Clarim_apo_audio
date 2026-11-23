//! Keeps one `clarim.exe agent` alive in the active console session.
//!
//! Runs on the service main thread. `Event`s come from the service control handler; the agent's
//! process handle is watched with a short `WaitForSingleObject` so stop requests are noticed.
//!
//! Exit-code contract with the agent: exit code 0 means "I was asked to quit" (tray "Sair"), and the
//! supervisor does NOT relaunch it until the next logon. Exit code 3 means "another agent already
//! runs in this session": it is relaunched, but only at the 30 s backoff cap. Any other exit (crash,
//! kill, non-zero) is relaunched with the normal backoff.

use std::ffi::{c_void, OsStr};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use windows::core::{HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_NO_TOKEN, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

/// How often the loop looks at the agent handle and the event channel.
const TICK: Duration = Duration::from_millis(250);
/// How long a terminated agent gets to disappear before we stop waiting for it.
const KILL_WAIT_MS: u32 = 3000;

/// What the service control handler tells the supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Service stop or system shutdown.
    Stop,
    /// A user logged on or attached to the console: make sure an agent runs there.
    Logon,
    /// The given session logged off: its agent dies with it, do not relaunch.
    Logoff(u32),
}

/// Relaunch delay: 2 s, doubling up to 30 s; back to 2 s once an agent stayed up for 60 s.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    const MIN: Duration = Duration::from_secs(2);
    const MAX: Duration = Duration::from_secs(30);
    const STABLE: Duration = Duration::from_secs(60);

    pub fn new() -> Self {
        Backoff { next: Self::MIN }
    }

    /// Delay before the next launch, given how long the previous attempt lasted
    /// (`Duration::ZERO` when it never started).
    pub fn on_exit(&mut self, uptime: Duration) -> Duration {
        if uptime >= Self::STABLE {
            self.next = Self::MIN;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        delay
    }
}

/// Exit code of an agent that found another agent already running (`EXIT_ALREADY_RUNNING` in apps/clarim).
const EXIT_ALREADY_RUNNING: u32 = 3;

/// Whether an ended agent is launched again (see the exit-code contract in the module docs).
fn should_relaunch(wanted: bool, exit_code: Option<u32>) -> bool {
    wanted && exit_code != Some(0)
}

/// Delay before relaunching an agent that ended with `exit_code` after `uptime`: the slowest pace
/// for "another agent already runs" (the existing one is not ours to fight), else the normal backoff.
fn relaunch_delay(backoff: &mut Backoff, exit_code: Option<u32>, uptime: Duration) -> Duration {
    if exit_code == Some(EXIT_ALREADY_RUNNING) {
        Backoff::MAX
    } else {
        backoff.on_exit(uptime)
    }
}

/// Appends `arg` to a command line as one argument, using the CommandLineToArgvW quoting rules.
pub fn quote_arg(arg: &OsStr) -> Vec<u16> {
    let (quote, backslash) = (u16::from(b'"'), u16::from(b'\\'));
    let mut out = vec![quote];
    let mut backslashes = 0;
    for c in arg.encode_wide() {
        if c == backslash {
            backslashes += 1;
            continue;
        }
        // Backslashes right before a quote must be doubled, and the quote itself escaped.
        let run = if c == quote { backslashes * 2 + 1 } else { backslashes };
        out.extend(std::iter::repeat_n(backslash, run));
        out.push(c);
        backslashes = 0;
    }
    // Backslashes right before the closing quote must be doubled too.
    out.extend(std::iter::repeat_n(backslash, backslashes * 2));
    out.push(quote);
    out
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

fn raw(h: &OwnedHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

/// Session of the physical console, or `None` when no session is attached to it.
fn console_session() -> Option<u32> {
    // SAFETY: takes no arguments and only reads a system value.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

struct EnvBlock(*mut c_void);

impl Drop for EnvBlock {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful CreateEnvironmentBlock and is freed once.
        unsafe {
            let _ = DestroyEnvironmentBlock(self.0);
        }
    }
}

struct Agent {
    process: OwnedHandle,
    pid: u32,
    session: u32,
    started: Instant,
}

/// Starts `exe agent` as the user of the console session. `Ok(None)` means nobody is logged on
/// there yet, which is normal at boot and not an error.
fn launch_agent(exe: &Path) -> Result<Option<Agent>> {
    let Some(session) = console_session() else {
        return Ok(None);
    };

    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer. The service runs as LocalSystem, which holds the
    // privilege WTSQueryUserToken requires.
    if let Err(e) = unsafe { WTSQueryUserToken(session, &mut token) } {
        if e.code() == HRESULT::from_win32(ERROR_NO_TOKEN.0) {
            return Ok(None);
        }
        return Err(anyhow!("WTSQueryUserToken(session {session}): {e}"));
    }
    // SAFETY: WTSQueryUserToken succeeded, so `token` is a new handle that only we own.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    spawn_as_user(&token, session, exe, "agent").map(Some)
}

/// Starts `exe args` with the primary `token`'s user, on `winsta0\default`, without a console window.
fn spawn_as_user(token: &OwnedHandle, session: u32, exe: &Path, args: &str) -> Result<Agent> {
    let mut env: *mut c_void = std::ptr::null_mut();
    // SAFETY: `env` is a valid out pointer and the token handle is open.
    unsafe { CreateEnvironmentBlock(&mut env, raw(token), false) }
        .map_err(|e| anyhow!("CreateEnvironmentBlock: {e}"))?;
    let env = EnvBlock(env);

    // CreateProcessAsUserW may write into the command line buffer, so it must be mutable.
    let mut cmd = quote_arg(exe.as_os_str());
    cmd.extend(format!(" {args}\0").encode_utf16());
    let mut desktop = wide(OsStr::new("winsta0\\default"));
    let cwd = wide(exe.parent().unwrap_or_else(|| Path::new(".")).as_os_str());
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();

    // SAFETY: every pointer refers to a live, NUL-terminated buffer (`cmd`, `desktop`, `cwd`) or
    // struct (`startup`, `info`) that outlives the call; the environment block was created for
    // this token and is Unicode, which CREATE_UNICODE_ENVIRONMENT declares.
    unsafe {
        CreateProcessAsUserW(
            raw(token),
            PCWSTR::null(),
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(env.0 as *const c_void),
            PCWSTR(cwd.as_ptr()),
            &startup,
            &mut info,
        )
    }
    .map_err(|e| anyhow!("CreateProcessAsUserW: {e}"))?;

    // SAFETY: CreateProcessAsUserW succeeded, so both handles are new and owned by us. The thread
    // handle is not needed; the process handle is what we watch.
    let (process, _thread) = unsafe {
        (OwnedHandle::from_raw_handle(info.hProcess.0), OwnedHandle::from_raw_handle(info.hThread.0))
    };
    Ok(Agent { process, pid: info.dwProcessId, session, started: Instant::now() })
}

/// True once the process has ended. Anything but a timeout counts as ended, so a broken handle
/// cannot turn the loop into a busy spin.
fn wait_exited(process: &OwnedHandle, timeout: Duration) -> bool {
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: the handle is a process handle we own and keep open for the call.
    unsafe { WaitForSingleObject(raw(process), ms) != WAIT_TIMEOUT }
}

fn exit_code(process: &OwnedHandle) -> Option<u32> {
    let mut code = 0u32;
    // SAFETY: the handle is a process handle we own and `code` is a valid out pointer.
    unsafe { GetExitCodeProcess(raw(process), &mut code) }.ok().map(|()| code)
}

struct Supervisor {
    exe: PathBuf,
    agent: Option<Agent>,
    backoff: Backoff,
    /// An agent should run (false after a logoff or a normal agent exit, until the next logon).
    wanted: bool,
    /// Earliest time of the next launch attempt.
    retry_at: Instant,
}

impl Supervisor {
    fn launch_if_due(&mut self) {
        if !self.wanted || self.agent.is_some() || Instant::now() < self.retry_at {
            return;
        }
        match launch_agent(&self.exe) {
            Ok(Some(agent)) => {
                log::info!("agent started (pid {}, session {})", agent.pid, agent.session);
                self.agent = Some(agent);
            }
            outcome => {
                match outcome {
                    Ok(_) => log::debug!("no interactive user on the console yet"),
                    Err(e) => log::warn!("could not start the agent: {e:#}"),
                }
                self.retry_at = Instant::now() + self.backoff.on_exit(Duration::ZERO);
            }
        }
    }

    fn agent_exited(&mut self) {
        let Some(agent) = self.agent.take() else { return };
        let code = exit_code(&agent.process);
        let uptime = agent.started.elapsed();
        log::info!("agent (pid {}) exited with code {code:?} after {uptime:?}", agent.pid);
        if !should_relaunch(self.wanted, code) {
            // Quit on purpose, another agent exists, or the session is going away.
            self.wanted = false;
            return;
        }
        let delay = relaunch_delay(&mut self.backoff, code, uptime);
        log::warn!("restarting the agent in {delay:?}");
        self.retry_at = Instant::now() + delay;
    }

    fn logon(&mut self) {
        self.wanted = true;
        self.backoff = Backoff::new();
        self.retry_at = Instant::now();
        // One agent follows the console: after a user switch the old session's agent goes away.
        let stale = match (&self.agent, console_session()) {
            (Some(a), Some(console)) => a.session != console,
            _ => false,
        };
        if stale {
            self.terminate_agent();
        }
    }

    fn logoff(&mut self, session: u32) {
        let ours = match &self.agent {
            Some(a) => a.session == session,
            None => console_session() == Some(session),
        };
        if ours {
            self.wanted = false; // the agent dies with its session; do not relaunch it
        }
    }

    fn terminate_agent(&mut self) {
        let Some(agent) = self.agent.take() else { return };
        // SAFETY: the handle is a process handle we own; it is closed when `agent` drops.
        unsafe {
            if let Err(e) = TerminateProcess(raw(&agent.process), 1) {
                log::warn!("TerminateProcess(agent pid {}): {e}", agent.pid);
            }
            WaitForSingleObject(raw(&agent.process), KILL_WAIT_MS);
        }
        log::info!("agent (pid {}) terminated", agent.pid);
    }
}

/// Supervisor loop; returns after `Event::Stop` with the agent terminated. `on_stopping` runs first,
/// the moment the stop is seen and before the agent is terminated (the service reports StopPending there).
pub fn run(exe: PathBuf, events: Receiver<Event>, on_stopping: impl FnOnce()) {
    let mut sup =
        Supervisor { exe, agent: None, backoff: Backoff::new(), wanted: true, retry_at: Instant::now() };
    loop {
        sup.launch_if_due();
        let exited = sup.agent.as_ref().is_some_and(|a| wait_exited(&a.process, TICK));
        if exited {
            sup.agent_exited();
        }
        // With an agent the wait above already took a tick; without one, sleep on the channel.
        let wait = if sup.agent.is_some() { Duration::ZERO } else { TICK };
        match events.recv_timeout(wait) {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::Logon) => sup.logon(),
            Ok(Event::Logoff(session)) => sup.logoff(session),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    on_stopping();
    sup.terminate_agent();
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_SYNCHRONIZE};

    fn quoted(s: &str) -> String {
        String::from_utf16_lossy(&quote_arg(OsStr::new(s)))
    }

    #[test]
    fn quotes_paths_with_spaces() {
        assert_eq!(quoted(r"C:\Program Files\Clarim\clarim.exe"), r#""C:\Program Files\Clarim\clarim.exe""#);
    }

    #[test]
    fn quoting_doubles_backslashes_before_quotes() {
        assert_eq!(quoted(r"C:\dir\"), r#""C:\dir\\""#);
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quoted(""), r#""""#);
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let mut b = Backoff::new();
        let secs: Vec<u64> = (0..7).map(|_| b.on_exit(Duration::from_secs(1)).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn backoff_resets_after_a_stable_run() {
        let mut b = Backoff::new();
        for _ in 0..4 {
            b.on_exit(Duration::from_secs(1));
        }
        assert_eq!(b.on_exit(Duration::from_secs(59)).as_secs(), 30);
        assert_eq!(b.on_exit(Duration::from_secs(60)).as_secs(), 2);
        assert_eq!(b.on_exit(Duration::from_secs(1)).as_secs(), 4);
    }

    /// The caller's own token as a primary token: `spawn_as_user` needs no privilege with it, unlike
    /// the WTSQueryUserToken token, which only LocalSystem can get. Everything else in the launch
    /// path (environment block, command line, desktop, flags, handles) is the production code.
    fn own_primary_token() -> OwnedHandle {
        let mut current = HANDLE::default();
        // SAFETY: valid out pointer; the pseudo handle from GetCurrentProcess needs no closing.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut current) }.unwrap();
        // SAFETY: OpenProcessToken succeeded, so the handle is new and owned here.
        let current = unsafe { OwnedHandle::from_raw_handle(current.0) };
        let mut primary = HANDLE::default();
        // SAFETY: valid in/out handles; no security attributes.
        unsafe { DuplicateTokenEx(raw(&current), TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut primary) }
            .unwrap();
        // SAFETY: DuplicateTokenEx succeeded, so the handle is new and owned here.
        unsafe { OwnedHandle::from_raw_handle(primary.0) }
    }

    fn has_ended(pid: u32) -> bool {
        // SAFETY: plain open and wait on a process handle that is closed right after.
        unsafe {
            match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                Err(_) => true, // already gone
                Ok(h) => {
                    let h = OwnedHandle::from_raw_handle(h.0);
                    WaitForSingleObject(raw(&h), 3000) == WAIT_OBJECT_0
                }
            }
        }
    }

    /// Drives the supervisor's state machine with a private copy of ping.exe (in a folder with
    /// spaces, so the quoting matters) standing in for the agent, launched through `spawn_as_user`:
    /// exit code 0 is a quit and is not relaunched, a crash is relaunched after the backoff, a logoff
    /// stops relaunching, and `terminate_agent` really ends the process.
    #[test]
    fn supervisor_reacts_to_agent_exits_logoff_and_stop() {
        let Some(system_root) = std::env::var_os("SystemRoot") else { return };
        let ping = Path::new(&system_root).join(r"System32\PING.EXE");
        if !ping.is_file() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("clarim svc spawn {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("clarim-spawn-test.exe");
        std::fs::copy(&ping, &exe).unwrap();
        let token = own_primary_token();
        let new_supervisor = |agent| Supervisor {
            exe: exe.clone(),
            agent: Some(agent),
            backoff: Backoff::new(),
            wanted: true,
            retry_at: Instant::now(),
        };
        let spawn = |args: &str| spawn_as_user(&token, 1, &exe, args).expect("spawn");

        // Exits by itself with code 0 (ping succeeds): a quit, no relaunch.
        let quick = spawn("-n 1 127.0.0.1");
        assert!(wait_exited(&quick.process, Duration::from_secs(10)), "quick process did not end");
        assert_eq!(exit_code(&quick.process), Some(0));
        let mut sup = new_supervisor(quick);
        sup.agent_exited();
        assert!(sup.agent.is_none() && !sup.wanted, "exit code 0 must not be relaunched");

        // Killed from outside (non-zero code, like a crash): relaunch after the first backoff step.
        let victim = spawn("-n 60 127.0.0.1");
        assert!(!wait_exited(&victim.process, Duration::from_millis(200)), "long process ended early");
        // SAFETY: the handle is a process handle we own; the exit code mimics a crash.
        unsafe { TerminateProcess(raw(&victim.process), 0xC000_0005) }.unwrap();
        assert!(wait_exited(&victim.process, Duration::from_secs(5)));
        let mut sup = new_supervisor(victim);
        let before = Instant::now();
        sup.agent_exited();
        assert!(sup.wanted, "a crash must be relaunched");
        assert!(sup.retry_at >= before + Duration::from_millis(1900), "first backoff step is 2 s");

        // Logoff of the agent's session: wanted drops, and stop terminates the process.
        let long = spawn("-n 60 127.0.0.1");
        let pid = long.pid;
        let mut sup = new_supervisor(long);
        sup.logoff(7); // some other session: nothing changes
        assert!(sup.wanted);
        sup.logoff(1);
        assert!(!sup.wanted, "logoff of the agent's session must stop relaunching");
        sup.terminate_agent();
        let ended = has_ended(pid);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(sup.agent.is_none());
        assert!(ended, "terminate_agent left the process running");
    }

    /// The real loop with an executable that cannot be launched (and, off LocalSystem, no user token
    /// either): failed launches must not spin or block, and a stop (or a dropped sender) ends it.
    #[test]
    fn loop_survives_failed_launches_and_ends_on_stop() {
        use std::sync::mpsc;
        let missing = || PathBuf::from(r"C:\clarim-test-does-not-exist\clarim.exe");

        let (tx, rx) = mpsc::channel();
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&stopping);
        let looping = std::thread::spawn(move || {
            run(missing(), rx, move || {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(!looping.is_finished(), "the loop must keep running until told to stop");
        assert_eq!(stopping.load(std::sync::atomic::Ordering::SeqCst), 0, "no stop notice before the stop");
        tx.send(Event::Logon).unwrap(); // a logon in between must not upset anything
        tx.send(Event::Stop).unwrap();
        let stopped_at = Instant::now();
        looping.join().unwrap();
        assert!(stopped_at.elapsed() < Duration::from_secs(2), "stop took too long");
        assert_eq!(stopping.load(std::sync::atomic::Ordering::SeqCst), 1, "the stop notice runs exactly once");

        let (tx, rx) = mpsc::channel::<Event>();
        let looping = std::thread::spawn(move || run(missing(), rx, || {}));
        drop(tx);
        looping.join().unwrap();
    }

    #[test]
    fn relaunch_policy() {
        assert!(should_relaunch(true, Some(1)));
        assert!(should_relaunch(true, Some(0xC000_0005))); // crash
        assert!(should_relaunch(true, None)); // exit code unreadable
        assert!(!should_relaunch(true, Some(0))); // quit on purpose
        assert!(!should_relaunch(false, Some(1))); // logged off
        // exit code 3 = another agent already runs: relaunched, but never faster than the cap
        assert!(should_relaunch(true, Some(EXIT_ALREADY_RUNNING)));
        assert!(!should_relaunch(false, Some(EXIT_ALREADY_RUNNING)));
    }

    #[test]
    fn already_running_agent_is_retried_at_the_backoff_cap() {
        let mut b = Backoff::new();
        let cap = Duration::from_secs(30);
        assert_eq!(relaunch_delay(&mut b, Some(EXIT_ALREADY_RUNNING), Duration::from_secs(1)), cap);
        // ...and does not use up the normal ladder: a crash right after still starts at 2 s
        assert_eq!(relaunch_delay(&mut b, Some(1), Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(relaunch_delay(&mut b, Some(EXIT_ALREADY_RUNNING), Duration::from_secs(1)), cap);
        assert_eq!(relaunch_delay(&mut b, None, Duration::from_secs(1)), Duration::from_secs(4));
    }
}
