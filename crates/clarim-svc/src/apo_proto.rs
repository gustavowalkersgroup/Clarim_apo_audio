//! The commands on the `\\.\pipe\ClarimEqApo` pipe, and the client functions the agent calls.
//!
//! One request per connection, one JSON line each way (`eqapo.rs` is the transport). Requests:
//!
//! | request line | what the service does |
//! |---|---|
//! | `{"entries":[{"guid":"{..}","bass_db":0,"mid_db":0,"treble_db":0}]}` | writes `clarim-eq.txt` (the original write-config command, unchanged) |
//! | `{"cmd":"status"}` | replies `{"ok":true,"status":{...}}` (`ApoServiceStatus`) |
//! | `{"cmd":"install_apo","device_guids":["{..}"]}` | installs Equalizer APO if missing, then attaches the devices; returns at once, runs on a worker |
//! | `{"cmd":"set_device","guid":"{..}","enabled":true}` | attaches / detaches APO on one endpoint, inline |
//! | `{"cmd":"restart_audio"}` | restarts Windows Audio and its dependents; returns at once, runs on a worker |
//!
//! Replies: `{"ok":true}`, `{"ok":false,"error":"<PT-BR text>"}`, or the status. While a worker runs, every
//! other long command is refused with the busy text, and `status` says what is going on. The pipe is open to
//! every interactive user, so each field is validated by the service (GUID shape, existing endpoint, list
//! length), and nothing in a request can name a path, a URL or a command.

use std::sync::Arc;

use anyhow::Result;
use clarim_core::OutputEq;
use serde::{Deserialize, Serialize};

use crate::apo_service::{ApoService, ApoServiceStatus, Platform};
use crate::eqapo::{self, expect_ok, exchange, Reply, PIPE_NAME};
use crate::user_error::user_text;

const MSG_BAD_REQUEST: &str = "Pedido inválido.";

/// Writes the output equalizer file for a list of `(endpoint GUID, EQ)` (production: `eqapo::apply_installed`).
pub(crate) type WriteConfig = dyn Fn(&[(String, OutputEq)]) -> Result<()>;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub(crate) enum Command {
    Status,
    InstallApo { device_guids: Vec<String> },
    SetDevice { guid: String, enabled: bool },
    RestartAudio,
}

fn reply_for(result: Result<()>, what: &str) -> Reply {
    match result {
        Ok(()) => {
            log::info!("pipe command {what}: accepted");
            Reply::ok()
        }
        Err(e) => {
            log::warn!("pipe command {what}: refused: {e:#}");
            Reply::failed(&user_text(&e, "O serviço do Clarim recusou o pedido."))
        }
    }
}

/// Answers one request line. A line without `cmd` is the original write-config request. Never logs a
/// request's content, only which command it was.
pub(crate) fn handle<P: Platform>(
    svc: &Arc<ApoService<P>>,
    write_config: &WriteConfig,
    line: &[u8],
) -> Reply {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else { return Reply::failed(MSG_BAD_REQUEST) };
    if value.get("cmd").is_none() {
        return eqapo::process(line, write_config);
    }
    // The serde message is not forwarded: it can quote the client's text.
    let Ok(command) = serde_json::from_value::<Command>(value) else { return Reply::failed(MSG_BAD_REQUEST) };
    match command {
        Command::Status => Reply { ok: true, error: None, status: Some(svc.status()) },
        Command::InstallApo { device_guids } => reply_for(svc.start_install(&device_guids), "install_apo"),
        Command::SetDevice { guid, enabled } => reply_for(svc.set_device(&guid, enabled), "set_device"),
        Command::RestartAudio => reply_for(svc.start_restart(), "restart_audio"),
    }
}

// ---------------------------------------------------------------------------------------------
// Client (used by the agent)
// ---------------------------------------------------------------------------------------------

fn command_via(pipe_name: &str, command: &Command) -> Result<Reply> {
    exchange(pipe_name, command)
}

fn status_via(pipe_name: &str) -> Result<ApoServiceStatus> {
    let reply = command_via(pipe_name, &Command::Status)?;
    let ok = reply.ok;
    match reply.status {
        Some(status) if ok => Ok(status),
        _ => expect_ok(reply).and_then(|()| Err(anyhow::anyhow!("Resposta inválida do serviço do Clarim."))),
    }
}

/// What the Clarim service knows about Equalizer APO and the playback endpoints. Cheap (a few registry
/// reads in the service); the agent calls it every few seconds. Errors are PT-BR.
pub fn apo_status() -> Result<ApoServiceStatus> {
    status_via(PIPE_NAME)
}

/// Asks the service to install Equalizer APO (when it is missing) and attach it to `device_guids` (endpoint
/// GUIDs `"{8-4-4-4-12}"`, at most 16, never virtual devices). Returns once the service has accepted the job;
/// follow it with `apo_status()` (`busy`, then `installed` / `error`). Errors are PT-BR, including "busy".
pub fn apo_install(device_guids: &[String]) -> Result<()> {
    expect_ok(command_via(PIPE_NAME, &Command::InstallApo { device_guids: device_guids.to_vec() })?)
}

/// Attaches (`enabled`) or detaches Equalizer APO on one endpoint. Done when it returns; the change only takes
/// effect after `apo_restart_audio()` (`status.needs_restart` says so). Errors are PT-BR.
pub fn apo_set_device(guid: &str, enabled: bool) -> Result<()> {
    expect_ok(command_via(PIPE_NAME, &Command::SetDevice { guid: guid.to_string(), enabled })?)
}

/// Asks the service to restart Windows Audio (the sound drops for a few seconds). Returns once the service has
/// accepted the job; follow it with `apo_status()`. Errors are PT-BR.
pub fn apo_restart_audio() -> Result<()> {
    expect_ok(command_via(PIPE_NAME, &Command::RestartAudio)?)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;

    use anyhow::bail;

    use super::*;
    use crate::apo_plan::Plan;
    use crate::apo_reg::EndpointInfo;
    use crate::audiodg::AudioDg;
    use crate::eqapo::Server;
    use crate::user_error::user_err;

    const GUID: &str = "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}";
    /// Everyone may use the test pipe: tests must not depend on being in an interactive session.
    const SDDL: &str = "D:(A;;GA;;;WD)";

    /// A machine with no endpoints that accepts every long operation silently.
    struct Quiet {
        restarts: AtomicU32,
    }

    impl Platform for Quiet {
        fn apo_installed(&self) -> bool {
            true
        }
        fn list_devices(&self) -> Result<Vec<EndpointInfo>> {
            Ok(Vec::new())
        }
        fn read_device(&self, guid: &str) -> Result<EndpointInfo> {
            if clarim_core::is_braced_guid(guid) {
                Err(user_err("Aparelho de áudio não encontrado."))
            } else {
                Err(user_err("Identificador de dispositivo inválido."))
            }
        }
        fn backup_device(&self, _: &str) -> Result<()> {
            bail!("unused")
        }
        fn apply_plan(&self, _: &str, _: &Plan) -> Result<()> {
            bail!("unused")
        }
        fn ensure_protected_audio_dg(&self) -> Result<bool> {
            Ok(false)
        }
        fn download_installer(&self, _: &AtomicBool) -> Result<PathBuf> {
            bail!("unused")
        }
        fn verify_installer(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn run_installer(&self, _: &Path, _: &AtomicBool) -> Result<()> {
            Ok(())
        }
        fn check_installed(&self) -> Result<()> {
            Ok(())
        }
        fn finish_install(&self, _: &Path) {}
        fn restart_audio(&self, _: &mut dyn FnMut(&str)) -> Result<Vec<String>> {
            self.restarts.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
        fn audiodg(&self) -> AudioDg {
            AudioDg { starts: Some(Vec::new()), apo_loaded: None }
        }
        fn now(&self) -> u64 {
            1
        }
    }

    fn service() -> Arc<ApoService<Quiet>> {
        ApoService::new(Quiet { restarts: AtomicU32::new(0) })
    }

    fn unique_pipe(tag: &str) -> String {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        format!(r"\\.\pipe\ClarimApoTest-{}-{tag}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    type Log = Arc<Mutex<Vec<String>>>;
    type Writer = Box<dyn Fn(&[(String, OutputEq)]) -> Result<()> + Send + Sync>;

    fn written() -> (Log, Writer) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        (log, Box::new(move |entries: &[(String, OutputEq)]| {
            sink.lock().unwrap().push(entries.iter().map(|(g, _)| g.clone()).collect::<Vec<_>>().join(","));
            Ok(())
        }))
    }

    fn ask(svc: &Arc<ApoService<Quiet>>, line: &str) -> Reply {
        let (_, write) = written();
        handle(svc, &write, line.as_bytes())
    }

    #[test]
    fn the_wire_format_of_every_command_is_fixed() {
        let json = |c: &Command| serde_json::to_string(c).unwrap();
        assert_eq!(json(&Command::Status), r#"{"cmd":"status"}"#);
        assert_eq!(json(&Command::RestartAudio), r#"{"cmd":"restart_audio"}"#);
        assert_eq!(
            json(&Command::InstallApo { device_guids: vec![GUID.into()] }),
            r#"{"cmd":"install_apo","device_guids":["{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}"]}"#
        );
        assert_eq!(
            json(&Command::SetDevice { guid: GUID.into(), enabled: true }),
            r#"{"cmd":"set_device","guid":"{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}","enabled":true}"#
        );
        let back: Command = serde_json::from_str(r#"{"cmd":"set_device","guid":"{x}","enabled":false}"#).unwrap();
        assert_eq!(back, Command::SetDevice { guid: "{x}".into(), enabled: false });
    }

    #[test]
    fn the_original_write_config_request_still_works_and_still_validates() {
        let svc = service();
        let (log, write) = written();
        let line = format!(r#"{{"entries":[{{"guid":"{GUID}","bass_db":5,"mid_db":0,"treble_db":0}}]}}"#);
        let reply = handle(&svc, &write, line.as_bytes());
        assert!(reply.ok && reply.status.is_none());
        assert_eq!(*log.lock().unwrap(), [GUID]);
        // garbage in any form is "invalid", and nothing is written
        for bad in ["", "not json", "[]", r#"{"entries":"x"}"#, r#"{"cmd":"format_c"}"#, r#"{"cmd":42}"#, r#"{"cmd":"set_device"}"#, r#"{"cmd":"set_device","guid":1,"enabled":true}"#] {
            let reply = handle(&svc, &write, bad.as_bytes());
            assert!(!reply.ok, "{bad}");
            assert_eq!(reply.error.as_deref(), Some("Pedido inválido."), "{bad}");
        }
        assert_eq!(log.lock().unwrap().len(), 1, "refused requests wrote nothing");
    }

    #[test]
    fn status_replies_with_the_status_and_commands_validate_their_fields() {
        let svc = service();
        let reply = ask(&svc, r#"{"cmd":"status"}"#);
        let status = reply.status.expect("status");
        assert!(reply.ok && status.installed && status.busy.is_none() && status.devices.is_empty());

        let refused = |line: &str| ask(&svc, line).error;
        assert_eq!(refused(r#"{"cmd":"set_device","guid":"{x}","enabled":true}"#).as_deref(), Some("Pedido inválido."));
        assert_eq!(
            refused(&format!(r#"{{"cmd":"set_device","guid":"{GUID}","enabled":true}}"#)).as_deref(),
            Some("Aparelho de áudio não encontrado.")
        );
        assert_eq!(refused(r#"{"cmd":"install_apo","device_guids":["nope"]}"#).as_deref(), Some("Pedido inválido."));
        let many = vec![format!("\"{GUID}\""); 17].join(",");
        assert_eq!(refused(&format!(r#"{{"cmd":"install_apo","device_guids":[{many}]}}"#)).as_deref(), Some("Pedido inválido."));
        assert!(ask(&svc, r#"{"cmd":"install_apo","device_guids":[]}"#).ok, "install with no devices is valid");
    }

    #[test]
    fn a_busy_service_says_what_it_is_doing_and_restart_is_rate_limited() {
        let svc = service();
        assert!(ask(&svc, r#"{"cmd":"restart_audio"}"#).ok);
        for _ in 0..200 {
            if svc.status().busy.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let again = ask(&svc, r#"{"cmd":"restart_audio"}"#);
        assert!(!again.ok && again.error.as_deref().is_some_and(|e| e.contains("acabou de ser reiniciado")), "{:?}", again.error);
    }

    #[test]
    fn commands_travel_over_the_real_pipe_in_both_directions() {
        let svc = service();
        let name = unique_pipe("e2e");
        let handler_svc = Arc::clone(&svc);
        let (log, write) = written();
        let write = Arc::new(write);
        let server = Server::start(&name, SDDL, move |line| handle(&handler_svc, &*write, line)).unwrap();

        let status = status_via(&name).expect("status over the pipe");
        assert!(status.installed && status.devices.is_empty());
        let refused = expect_ok(command_via(&name, &Command::SetDevice { guid: "{bad}".into(), enabled: true }).unwrap());
        assert_eq!(refused.unwrap_err().to_string(), "Pedido inválido.");
        let missing = expect_ok(command_via(&name, &Command::SetDevice { guid: GUID.into(), enabled: false }).unwrap());
        assert_eq!(missing.unwrap_err().to_string(), "Aparelho de áudio não encontrado.");
        expect_ok(command_via(&name, &Command::InstallApo { device_guids: vec![] }).unwrap()).expect("accepted");
        // the original write-config client still talks to the same server
        crate::eqapo::eqapo_send_via_for_tests(&name, &[(GUID.to_string(), OutputEq { bass_db: 3.0, ..OutputEq::default() })]).expect("write-config");
        assert_eq!(*log.lock().unwrap(), [GUID]);
        drop(server);
        let gone = status_via(&name).unwrap_err().to_string();
        assert_eq!(gone, "O serviço do Clarim não está em execução.");
    }
}
