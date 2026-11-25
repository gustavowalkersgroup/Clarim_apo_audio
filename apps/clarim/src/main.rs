//! `clarim.exe`: one binary, several subcommands (see `USAGE`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{bail, Result};
use clarim_core::DeviceInfo;
use std::process::ExitCode;

mod agent;
mod backend;
mod callwatch;
mod eqapo;
mod instance;
mod logging;
mod open;
mod tray;

const USAGE: &str = "\
Clarim: mic AI noise suppression into a virtual mic + per-app output channels

usage: clarim [command] [flags]

commands:
  (none)          open the dashboard if an agent runs, else run the agent in this process
  agent           run the agent (what the Windows service launches)
  open            open the dashboard window (the agent must be running)
  install         install and start the Windows service (needs admin)
  uninstall       stop and remove the Windows service (needs admin)
  service         service entry point (started by the Service Control Manager only)
  devices         list audio endpoints
  sessions        list the apps that play audio (read-only)
  selftest        benchmark the mic DSP on synthetic audio (touches no devices)
  reset-routing   clear EVERY per-app output route in Windows (needs --yes)

flags:
  --dry-run       agent: observe only, no app routing or volume changes, mic engine off
  --no-routing    agent: observe only for app routing and volume; mic engine stays on
  --yes, -y       reset-routing: confirm (there is no prompt: without it, nothing is cleared)

The agent logs to %LOCALAPPDATA%\\Clarim\\clarim.log; install and uninstall log to %ProgramFiles%\\Clarim\\install.log.
exit codes (agent): 0 quit on purpose, 3 another agent already runs, 1 failure.
";

const RESET_ROUTING_WARNING: &str = "\
Isto apaga TODAS as escolhas de saída de áudio por programa do Windows, inclusive as feitas em \
Configurações do Windows ou por outras ferramentas. Para continuar, execute: clarim reset-routing --yes";

/// Printed after `reset-routing --yes` while an agent runs: with "Roteamento por app" on it writes the
/// channel routes again within a second (and gives up after a few tries), so the reset would not last.
const RESET_ROUTING_AGENT_NOTE: &str = "\
Aviso: o Clarim está rodando e vai gravar as rotas de novo em alguns segundos. Desligue \"Roteamento por app\" \
no painel (ou feche o Clarim em \"Sair\" na bandeja) e rode este comando outra vez.";

const INSTALLED_MESSAGE: &str = "Clarim instalado. Ele inicia sozinho quando você entra no Windows. \
Abra o painel pelo menu Iniciar (Clarim) ou pelo ícone na bandeja.";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AgentFlags {
    dry_run: bool,
    no_routing: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// No subcommand: dashboard if an agent exists, else the agent in this process.
    Default(AgentFlags),
    Agent(AgentFlags),
    Open,
    Install,
    Uninstall,
    Service,
    Devices,
    Sessions,
    Selftest,
    ResetRouting { yes: bool },
    Help,
}

fn parse(args: &[String]) -> Result<Command> {
    let (cmd, rest) = match args.split_first() {
        Some((first, rest)) if !first.starts_with('-') => (Some(first.as_str()), rest),
        _ => (None, args),
    };
    let mut flags = AgentFlags::default();
    let mut yes = false;
    for a in rest {
        match a.as_str() {
            "--dry-run" => flags.dry_run = true,
            "--no-routing" => flags.no_routing = true,
            "--yes" | "-y" => yes = true,
            "-h" | "--help" => return Ok(Command::Help),
            other => bail!("unknown argument `{other}` (try --help)"),
        }
    }
    let takes_flags = matches!(cmd, None | Some("agent"));
    if !takes_flags && flags != AgentFlags::default() {
        bail!("--dry-run and --no-routing only apply to `agent` and to the no-argument mode");
    }
    if yes && cmd != Some("reset-routing") {
        bail!("--yes only applies to `reset-routing`");
    }
    Ok(match cmd {
        None => Command::Default(flags),
        Some("agent") => Command::Agent(flags),
        Some("open") => Command::Open,
        Some("install") => Command::Install,
        Some("uninstall") => Command::Uninstall,
        Some("service") => Command::Service,
        Some("devices") => Command::Devices,
        Some("sessions") => Command::Sessions,
        Some("selftest") => Command::Selftest,
        Some("reset-routing") => Command::ResetRouting { yes },
        Some("help") => Command::Help,
        Some(other) => bail!("unknown command `{other}` (try --help)"),
    })
}

/// Release builds are GUI-subsystem, so a console launch has no stdio. Attach to the parent
/// console (silently does nothing from Explorer or the service) and bind any unset std handle
/// (stdin, stdout, stderr) to it. Handles the parent redirected (pipe or file) are kept.
fn attach_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    // SAFETY: AttachConsole takes no pointers. GetStdHandle only reads this process's handle
    // table, and SetStdHandle is given a handle we just opened and deliberately leak, so it stays
    // valid for the life of the process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return; // no parent console (or this build already has one): nothing to attach to
        }
        for (which, device, write) in [
            (STD_INPUT_HANDLE, "CONIN$", false),
            (STD_OUTPUT_HANDLE, "CONOUT$", true),
            (STD_ERROR_HANDLE, "CONOUT$", true),
        ] {
            if !GetStdHandle(which).map_or(true, |h| h.is_invalid()) {
                continue;
            }
            if let Ok(f) = std::fs::OpenOptions::new().read(!write).write(write).open(device) {
                let _ = SetStdHandle(which, HANDLE(f.into_raw_handle()));
            }
        }
    }
}

fn print_devices(title: &str, devs: &[DeviceInfo], deny: &[String]) {
    println!("{title}");
    if devs.is_empty() {
        println!("  (none)");
    }
    for d in devs {
        let mut tags = Vec::new();
        if d.is_default {
            tags.push("default");
        }
        if d.is_virtual {
            tags.push("virtual");
        }
        if clarim_audio::devices::is_denied(&d.name, deny) {
            tags.push("hidden by deny-list");
        }
        let tags = if tags.is_empty() { String::new() } else { format!("  [{}]", tags.join(", ")) };
        println!("  {}{tags}\n      id {}", d.name, d.id);
    }
}

/// Read-only: lists every endpoint (deny-listed ones flagged, not hidden) and what Clarim would pick.
fn devices() -> Result<()> {
    clarim_audio::init_com()?;
    let deny = clarim_core::Config::load().deny_devices;
    let inputs = clarim_audio::list_inputs(&[])?;
    let outputs = clarim_audio::list_outputs(&[])?;
    print_devices("Capture endpoints (microphones):", &inputs, &deny);
    print_devices("Render endpoints (playback):", &outputs, &deny);

    let visible = |devs: &[DeviceInfo]| -> Vec<DeviceInfo> {
        devs.iter().filter(|d| !clarim_audio::devices::is_denied(&d.name, &deny)).cloned().collect()
    };
    let (inputs, outputs) = (visible(&inputs), visible(&outputs));
    println!();
    match clarim_audio::pick_real_mic(&inputs) {
        Some(d) => println!("real mic Clarim would use: {}", d.name),
        None => println!("real mic Clarim would use: none found"),
    }
    match clarim_audio::find_virtual_mic(&outputs) {
        Some(d) => println!("virtual mic target:        {}", d.name),
        None => println!("virtual mic target:        none found - install VB-Cable (https://vb-audio.com/Cable/)"),
    }
    Ok(())
}

/// Read-only: the apps that currently have an audio session and the channel the saved rules put them in.
fn sessions() -> Result<()> {
    let cfg = clarim_core::Config::load();
    let observe = clarim_routing::RoutingConfig {
        channels: cfg.channels,
        app_rules: cfg.app_rules,
        default_channel: cfg.default_channel,
        enabled: false,
        call_to_chat: cfg.call_to_chat,
    };
    let sessions = clarim_routing::list_sessions_once(&observe)?;
    let _ = clarim_audio::init_com();
    let names: std::collections::HashMap<String, String> = clarim_audio::list_outputs(&[])
        .map(|v| v.into_iter().map(|d| (d.id, d.name)).collect())
        .unwrap_or_default();

    println!("Audio sessions (read-only, nothing is changed):");
    println!("  {:<7} {:<28} {:<8} {:<9} {:<5} endpoint", "pid", "exe", "channel", "state", "peak");
    for s in &sessions {
        let channel = if s.exe == clarim_routing::UNKNOWN_EXE {
            "-".to_string() // never routed: the process cannot be inspected
        } else {
            format!("{:?}{}", s.channel, if s.has_rule { "" } else { " *" }).to_lowercase()
        };
        let endpoint = names.get(&s.device_id).unwrap_or(&s.device_id);
        let state = if s.active { "active" } else { "inactive" };
        let call = if s.in_call { "  [in call: plays on chat]" } else { "" };
        println!("  {:<7} {:<28} {:<8} {:<9} {:<5.2} {endpoint}{call}", s.pid, s.exe, channel, state, s.peak);
    }
    println!("{} app(s). `*` = no rule, uses the default channel ({:?}).", sessions.len(), cfg.default_channel);
    Ok(())
}

/// No stdin prompt: the release exe is GUI-subsystem, so it would race the shell for console input.
/// Without `--yes` it only explains and fails (exit code 1).
fn reset_routing(yes: bool) -> Result<()> {
    if !yes {
        bail!("{RESET_ROUTING_WARNING}");
    }
    clarim_routing::clear_all_app_routes()?;
    println!("{}", reset_routing_report(instance::agent_running()));
    Ok(())
}

/// What `reset-routing --yes` prints once the routes are cleared.
fn reset_routing_report(agent_running: bool) -> String {
    let done = "all per-app output routes cleared";
    if agent_running {
        format!("{done}\n{RESET_ROUTING_AGENT_NOTE}")
    } else {
        done.to_string()
    }
}

fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Command::Devices => devices(),
        Command::Sessions => sessions(),
        Command::Selftest => {
            print!("{}", clarim_audio::selftest()?);
            Ok(())
        }
        Command::Install => {
            clarim_svc::install()?;
            println!("{INSTALLED_MESSAGE}");
            Ok(())
        }
        Command::Uninstall => clarim_svc::uninstall(),
        Command::Service => clarim_svc::run_service(),
        Command::ResetRouting { yes } => reset_routing(yes),
        Command::Agent(flags) => agent::run(flags, false),
        Command::Default(flags) => {
            if instance::agent_running() {
                open::open_dashboard(clarim_core::Config::load().port)
            } else {
                agent::run(flags, true)
            }
        }
        Command::Open => {
            if !instance::agent_running() {
                bail!("Clarim is not running: start it with `clarim` (or install the service) first");
            }
            open::open_dashboard(clarim_core::Config::load().port)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        attach_console();
    }
    let cmd = match parse(&args) {
        Ok(cmd) => cmd,
        Err(e) => {
            logging::init(false);
            eprintln!("clarim: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    // Long-running modes and install/uninstall log to a file (a GUI-subsystem exe, an elevated run especially,
    // often has no console to print an error on; the file is what survives). The log is mirrored to stderr.
    let installing = matches!(cmd, Command::Install | Command::Uninstall);
    let to_file = installing || matches!(cmd, Command::Default(_) | Command::Agent(_) | Command::Service);
    if installing {
        logging::init_install();
    } else {
        logging::init(to_file);
    }
    match run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is::<agent::AlreadyRunning>() => {
            log::info!("{e}; exiting with code {}", agent::EXIT_ALREADY_RUNNING);
            ExitCode::from(agent::EXIT_ALREADY_RUNNING)
        }
        Err(e) => {
            if to_file {
                log::error!("{e:#}");
            } else {
                eprintln!("clarim: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_subcommands() {
        assert_eq!(p(&[]).unwrap(), Command::Default(AgentFlags::default()));
        assert_eq!(p(&["open"]).unwrap(), Command::Open);
        assert_eq!(p(&["install"]).unwrap(), Command::Install);
        assert_eq!(p(&["uninstall"]).unwrap(), Command::Uninstall);
        assert_eq!(p(&["service"]).unwrap(), Command::Service);
        assert_eq!(p(&["devices"]).unwrap(), Command::Devices);
        assert_eq!(p(&["sessions"]).unwrap(), Command::Sessions);
        assert_eq!(p(&["selftest"]).unwrap(), Command::Selftest);
        assert_eq!(p(&["reset-routing"]).unwrap(), Command::ResetRouting { yes: false });
        assert_eq!(p(&["reset-routing", "--yes"]).unwrap(), Command::ResetRouting { yes: true });
        assert_eq!(p(&["--help"]).unwrap(), Command::Help);
    }

    #[test]
    fn parses_agent_flags_in_any_position() {
        let both = AgentFlags { dry_run: true, no_routing: true };
        assert_eq!(p(&["agent", "--dry-run", "--no-routing"]).unwrap(), Command::Agent(both));
        assert_eq!(p(&["--no-routing", "--dry-run"]).unwrap(), Command::Default(both));
        assert_eq!(
            p(&["agent", "--no-routing"]).unwrap(),
            Command::Agent(AgentFlags { dry_run: false, no_routing: true })
        );
    }

    #[test]
    fn reset_routing_without_yes_only_explains_and_fails() {
        // Never reaches clear_all_app_routes: nothing in Windows is touched.
        let err = reset_routing(false).expect_err("must refuse without --yes");
        let msg = err.to_string();
        assert!(msg.contains("TODAS") && msg.contains("clarim reset-routing --yes"), "{msg}");
    }

    #[test]
    fn reset_routing_warns_when_an_agent_would_undo_it() {
        let alone = reset_routing_report(false);
        assert_eq!(alone, "all per-app output routes cleared");
        let with_agent = reset_routing_report(true);
        assert!(with_agent.starts_with(&alone), "the success line stays first");
        assert!(with_agent.contains("Roteamento por app") && with_agent.contains("rode este comando outra vez"), "{with_agent}");
    }

    #[test]
    fn rejects_unknown_input() {
        assert!(p(&["frobnicate"]).is_err());
        assert!(p(&["agent", "--bogus"]).is_err());
        assert!(p(&["devices", "--dry-run"]).is_err());
        assert!(p(&["agent", "--yes"]).is_err());
        assert!(p(&["--yes"]).is_err());
    }
}
