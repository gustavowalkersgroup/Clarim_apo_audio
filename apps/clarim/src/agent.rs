//! The agent: what the Windows service launches in the user's session (or `clarim` runs in-process
//! in dev mode). One instance per user session; owns the engines, the dashboard server and the tray.
//!
//! Exit contract with the service supervisor: `Ok` (exit code 0) means "quit on purpose" (tray
//! *Sair*) and is not relaunched. `AlreadyRunning` (exit code 3) means another agent holds the
//! session mutex; the supervisor retries at its slowest pace. Any other `Err` (exit code 1) is a
//! failure and is relaunched with backoff.

use std::fmt;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clarim_core::Config;

use crate::backend::{first_run_defaults, scan_devices, AppBackend, Scan};
use crate::tray::{self, TrayAction, TrayEvent};
use crate::{instance, open, AgentFlags};

/// Process exit code of an agent that found another agent already running. The service supervisor
/// (`clarim-svc`, `EXIT_ALREADY_RUNNING`) relaunches it at the 30 s backoff cap.
pub const EXIT_ALREADY_RUNNING: u8 = 3;

/// Returned by `run` when another agent holds the session mutex; `main` maps it to `EXIT_ALREADY_RUNNING`.
#[derive(Debug)]
pub struct AlreadyRunning;

impl fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("another Clarim agent already runs in this session")
    }
}

impl std::error::Error for AlreadyRunning {}

/// Fails when nothing can listen on the dashboard port. Checked before the engines start, so a busy
/// port never turns the mic and the routing on only to tear them down again in a restart loop.
fn check_port_free(port: u16) -> Result<()> {
    TcpListener::bind(("127.0.0.1", port)).map(drop).with_context(|| {
        format!("dashboard port {port} is busy: another program is using it (a porta {port} do painel está ocupada)")
    })
}

/// First-run defaults (no routing next to Sonar, channels off a virtual default device) need a device
/// scan that worked: after a failed one they would be computed from an empty list and saved, and the
/// file existing would end the first run for good. A failed scan leaves the config file out, so the
/// next start tries again.
fn is_first_run(config_exists: bool, scan_ok: bool) -> bool {
    !config_exists && scan_ok
}

/// Runs until the user picks "Sair" in the tray. `open_dashboard` = also open the panel once started.
pub fn run(flags: AgentFlags, open_dashboard: bool) -> Result<()> {
    let Some(_guard) = instance::acquire()? else {
        return Err(AlreadyRunning.into());
    };
    let mut cfg = Config::load();
    let port = cfg.port;
    check_port_free(port)?;
    // Device scan on its own thread: COM (MTA) stays off the main thread, which owns the tray window.
    let scanned = std::thread::spawn(scan_devices).join().map_err(|_| anyhow!("the device scan panicked"))?;
    let scan_ok = scanned.is_ok();
    let scan = scanned.unwrap_or_else(|e| {
        log::warn!("cannot list audio devices: {e:#}");
        Scan::default()
    });
    if is_first_run(Config::path().exists(), scan_ok) {
        first_run_defaults(&mut cfg, &scan.outputs);
        match cfg.save() {
            Ok(()) => log::info!("first run: wrote {}", Config::path().display()),
            Err(e) => log::warn!("cannot write the first-run config: {e:#}"),
        }
    } else if !scan_ok && !Config::path().exists() {
        log::warn!("first-run defaults postponed: they need a working device scan, nothing was saved");
    }
    // What is in force: the overrides never reach the saved config (see `AppBackend::effective_config`).
    log::info!(
        "Clarim {} starting (dry_run={}, no_routing={}, routing_enabled={}, mic_enabled={})",
        env!("CARGO_PKG_VERSION"),
        flags.dry_run,
        flags.no_routing,
        cfg.routing_enabled && !flags.dry_run && !flags.no_routing,
        cfg.mic.enabled && !flags.dry_run
    );

    let backend = Arc::new(AppBackend::new(cfg, flags, scan, Config::save));
    backend.init_eq_apo();
    let server = clarim_web::serve(backend.clone(), port)
        .with_context(|| format!("cannot start the dashboard on port {port} (is another program using it?)"));
    let server = match server {
        Ok(handle) => handle,
        Err(e) => {
            backend.shutdown();
            return Err(e);
        }
    };
    if let Err(e) = backend.spawn_housekeeping() {
        backend.shutdown();
        return Err(e);
    }
    // The server thread only ends if its listener dies. Then quit with a failure, so the service
    // supervisor restarts the agent.
    let server_died = Arc::new(AtomicBool::new(false));
    let watch = {
        let server_died = Arc::clone(&server_died);
        std::thread::Builder::new().name("clarim-web-watch".into()).spawn(move || {
            let _ = server.join();
            log::error!("the dashboard server stopped");
            server_died.store(true, Ordering::SeqCst);
            tray::request_exit();
        })
    };
    if let Err(e) = watch {
        log::warn!("cannot watch the dashboard server: {e}");
    }

    tray::set_mic_paused(backend.mic_paused());
    if open_dashboard {
        if let Err(e) = open::open_dashboard(port) {
            log::warn!("cannot open the dashboard: {e:#}");
        }
    }
    let shown = tray::run(|event| match event {
        TrayEvent::Open => {
            if let Err(e) = open::open_dashboard(port) {
                log::warn!("cannot open the dashboard: {e:#}");
            }
            TrayAction::None
        }
        TrayEvent::ToggleMic => match backend.toggle_mic() {
            Ok(enabled) => TrayAction::SetMicPaused(!enabled),
            Err(e) => {
                log::warn!("tray: {e:#}");
                TrayAction::None
            }
        },
        TrayEvent::Quit => TrayAction::Exit,
    });
    if let Err(e) = shown {
        // No tray (no shell, no desktop): keep the audio running instead of making the supervisor
        // restart the agent over and over. `clarim open` still reaches the dashboard. Only a dead
        // dashboard server ends this, with the failure exit below.
        log::error!("tray icon unavailable ({e:#}); running without it");
        while !server_died.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    backend.shutdown();
    if server_died.load(Ordering::SeqCst) {
        bail!("the dashboard server stopped unexpectedly");
    }
    log::info!("Clarim stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_port_is_reported_before_anything_starts() {
        // Hold an ephemeral port, then ask for it: the check must refuse with both languages in the message.
        let held = TcpListener::bind(("127.0.0.1", 0)).expect("bind an ephemeral port");
        let port = held.local_addr().expect("local addr").port();
        let err = check_port_free(port).expect_err("the port is taken");
        let msg = format!("{err:#}");
        assert!(msg.contains(&format!("port {port} is busy")) && msg.contains("está ocupada"), "{msg}");
        drop(held);
        assert!(check_port_free(port).is_ok(), "the port is free again once the listener is gone");
    }

    #[test]
    fn first_run_waits_for_a_working_device_scan() {
        assert!(is_first_run(false, true), "no config yet, devices known: first run");
        assert!(!is_first_run(false, false), "no config but the scan failed: retry on the next start, save nothing");
        assert!(!is_first_run(true, true), "a config exists: not the first run");
        assert!(!is_first_run(true, false));
    }

    #[test]
    fn already_running_is_its_own_error_and_exit_code() {
        let err = anyhow::Error::from(AlreadyRunning);
        assert!(err.is::<AlreadyRunning>());
        assert_eq!(EXIT_ALREADY_RUNNING, 3);
    }
}
