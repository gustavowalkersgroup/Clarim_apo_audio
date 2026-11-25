//! `clarim setup`: the guided install for people who only downloaded `clarim.exe` and double-clicked it.
//!
//! One UAC prompt, then everything: VB-Cable (the virtual microphone), the Clarim service (the existing
//! `clarim_svc::install`) and Equalizer APO (the output equalizer), each only when it is missing, and a PT-BR summary
//! at the end.
//!
//! Two processes take part. The launcher (the user's normal, not elevated process: a double click, or `clarim setup`
//! typed in a normal shell) asks Windows for administrator rights by starting `clarim setup --no-open` elevated
//! (`ShellExecuteExW` with "runas"), waits for it, and then opens the dashboard itself. It is the launcher that opens
//! it because a browser started from the elevated process would itself run elevated, in the user's own Edge profile.
//! The elevated process does the work and shows the dialogs. A process that is already elevated (`clarim.exe` run as
//! administrator) does the work in place and opens nothing, for the same reason.
//!
//! One at a time: the launcher holds `instance::acquire_setup_launcher`, the elevated process `instance::acquire_setup`.
//!
//! The service is installed only when it is missing: one that is there is left alone (started when stopped), so a running
//! Clarim is never stopped and a newer installed exe never replaced by an older download. A first install that fails after
//! the service was registered is undone with `clarim_svc::uninstall`, so that the next run starts clean.
//!
//! - Equalizer APO is installed by asking the freshly started service over its pipe (`apo_install`, the very job the
//!   dashboard's "Instalar equalizador" button starts), not by calling the installer code here: the service already
//!   sequences download, SHA-256 check, silent install, neutralising the stock `config.txt` (no -6 dB) and attaching the
//!   devices, with backups and a cancel on stop. Nothing would be gained by a second copy of that, and the setup process
//!   is not LocalSystem.
//! - VB-Cable is not installed silently: its licence forbids bundling it in another installer, so VB-Audio's own
//!   installer is shown and the user clicks through it (`clarim_svc::vbcable`).

use std::ffi::OsStr;
use std::mem::size_of;
use std::net::{SocketAddr, TcpStream};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clarim_core::{Config, DeviceInfo};
use clarim_svc::vbcable::{self, Package, VBCABLE_PAGE};
use clarim_svc::{user_text, ApoServiceStatus};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_CANCELLED, HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, SW_SHOWNORMAL,
};

use crate::instance::{self, InstanceGuard};
use crate::{backend, eqapo, open};

const TITLE: &str = "Clarim";

const OFFER_TEXT: &str = "Instalar o Clarim neste computador? (recomendado)\n\n\
Vai instalar o serviço do Clarim, o VB-Cable (microfone virtual, da VB-Audio) e o Equalizer APO, só o que ainda não estiver no PC. \
Leva alguns minutos e precisa de internet. No fim, o áudio do Windows pode reiniciar (o som cai por alguns segundos). \
O Windows pode pedir permissão de administrador uma vez.";

const UAC_DECLINED_TEXT: &str = "A instalação foi cancelada: o Windows não deu permissão de administrador.\n\n\
Para instalar, abra o Clarim de novo e clique em Sim (ou rode \"clarim.exe setup\").";

const ALREADY_RUNNING_TEXT: &str = "A instalação do Clarim já está em andamento.\n\n\
Responda ou termine a janela dela antes de tentar de novo.";

const CANNOT_START_TEXT: &str = "Não foi possível começar a instalação do Clarim. Tente de novo.";

const UAC_FAILED_TEXT: &str = "Não foi possível pedir permissão de administrador ao Windows.\n\n\
Tente de novo: clique com o botão direito no clarim.exe e escolha \"Executar como administrador\".";

const VBCABLE_INTRO_TEXT: &str = "Agora vai abrir o instalador do VB-Cable, da VB-Audio. \
Clique em \"Install Driver\" e depois em OK.\n\n\
O VB-Cable é doação (donationware): se gostar, apoie em vb-cable.com.";

/// What the launcher starts elevated: the same exe, `setup`, told not to open the dashboard (the launcher does).
const ELEVATED_ARGS: &str = "setup --no-open";

/// Exit code of the elevated setup when the VB-Cable package changed on VB-Audio's site and nothing was run: the
/// launcher then opens VB-Cable's page in the user's own (not elevated) browser. (1 is any failure, 3 is the agent's
/// "already running".)
const EXIT_VBCABLE_MANUAL: u8 = 4;

/// How long the launcher waits for the elevated copy: the sum of its own limits (downloads, VB-Audio's installer, the Equalizer
/// APO job, the audio restart) is well under this; the rest is the user at the dialogs. Past it the launcher gives up waiting
/// (it does not stop the elevated copy).
const SETUP_WAIT_MS: u32 = 45 * 60 * 1000;
/// How long the launcher waits for the dashboard's server after the install (the agent starts in the user's session
/// a moment after the service, and loads its models before it listens).
const DASHBOARD_WAIT: Duration = Duration::from_secs(40);
/// A freshly started service opens its pipe within seconds.
const SERVICE_WAIT: Duration = Duration::from_secs(30);
/// Download (up to 5 min) + silent install (up to 5 min) + attaching the devices, all inside the service.
const APO_WAIT: Duration = Duration::from_secs(12 * 60);
const RESTART_WAIT: Duration = Duration::from_secs(3 * 60);
/// The "CABLE Input" device appears a moment after VB-Audio's installer closes.
const CABLE_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_secs(1);

/// Returned by `run` when something is left to do (`main` exits with `0`'s code).
#[derive(Debug)]
pub struct Incomplete(pub u8);

impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the setup left something to do (see the summary); exit code {}", self.0)
    }
}

impl std::error::Error for Incomplete {}

// ------------------------------------------------------------------------------------------ decisions

/// `clarim` with no arguments: offer the guided install only when nothing of Clarim is there (no service, no running
/// agent) and the launch is a plain one (`clarim --dry-run` is a developer asking for the agent in this process).
pub fn should_offer(service_installed: bool, agent_running: bool, plain_launch: bool) -> bool {
    !service_installed && !agent_running && plain_launch
}

/// The "CABLE Input" playback device that VB-Cable adds. Not just any virtual mic: the dashboard and the README tell the user
/// to pick "CABLE Output" in the call app, so that is the one the setup looks for (same lookup the mic engine uses).
fn has_cable_input(outputs: &[DeviceInfo]) -> bool {
    clarim_audio::find_virtual_mic(outputs).is_some_and(|d| d.name.to_lowercase().contains("cable input"))
}

/// After the service ran the Equalizer APO job: did it end well, with a warning, or not at all?
fn apo_outcome(after: &ApoServiceStatus) -> Apo {
    match (after.installed, after.error.as_deref()) {
        (true, None) => Apo::Installed,
        (true, Some(warning)) => Apo::Warning(warning.to_string()),
        (false, Some(error)) => Apo::Failed(error.to_string()),
        (false, None) => Apo::Failed("A instalação do Equalizer APO não terminou.".to_string()),
    }
}

/// Windows Audio restarts once, at the end, and only for a change this setup made.
fn needs_audio_restart(installed_now: bool, after: &ApoServiceStatus) -> bool {
    installed_now && after.installed && after.needs_restart
}

// ------------------------------------------------------------------------------------------ outcome and summary

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cable {
    Present,
    Installed,
    /// VB-Audio's installer ran, but the device is not there yet.
    NeedsRestart,
    /// The package at the pinned URL has another hash: nothing was run.
    Changed,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Apo {
    Present,
    Installed,
    /// Installed, but the service says something went wrong afterwards (a device that could not be attached...): its PT-BR sentence.
    Warning(String),
    Failed(String),
    /// Not tried: the service is not there.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Audio {
    NotNeeded,
    Restarted,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    /// It was already installed (left alone) and is running.
    Present,
    Installed,
    /// Not installed, and nothing of the failed attempt is left registered.
    Failed,
    /// A service is registered but does not work: a first install that failed and could not be undone, or an installed one that
    /// would not start. Only `clarim.exe uninstall` and then `setup` clean that.
    Stuck,
}

impl Service {
    fn ok(self) -> bool {
        matches!(self, Service::Present | Service::Installed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    service: Service,
    cable: Cable,
    apo: Apo,
    audio: Audio,
}

impl Outcome {
    /// 0 = nothing left that Clarim could not do itself; `EXIT_VBCABLE_MANUAL`; else 1.
    fn exit_code(&self) -> u8 {
        if self.cable == Cable::Changed {
            EXIT_VBCABLE_MANUAL
        } else if !self.service.ok() || matches!(self.cable, Cable::Failed(_)) || matches!(self.apo, Apo::Failed(_) | Apo::Skipped) {
            1
        } else {
            0
        }
    }
}

/// `text` as a sentence: trimmed, and ended with a full stop unless it already has one.
fn sentence(text: &str) -> String {
    let t = text.trim();
    if t.ends_with(['.', '!', '?']) {
        t.to_string()
    } else {
        format!("{t}.")
    }
}

struct Summary {
    text: String,
    /// Something is left to do: the dialog gets the warning icon.
    warn: bool,
}

fn summary(o: &Outcome, install_log: &str) -> Summary {
    let mut done: Vec<String> = Vec::new();
    let mut todo: Vec<String> = Vec::new();
    match o.service {
        Service::Present => done.push("Clarim: o serviço já estava instalado (ele inicia sozinho com o Windows).".into()),
        Service::Installed => done.push("Clarim instalado como serviço (ele inicia sozinho com o Windows).".into()),
        Service::Failed => todo.push(format!("Não foi possível instalar o serviço do Clarim. O detalhe está no fim do arquivo {install_log}.")),
        Service::Stuck => todo.push(format!(
            "O serviço do Clarim está instalado, mas não está funcionando. Para refazer, abra um prompt como administrador e rode \
             \"clarim.exe uninstall\" e depois \"clarim.exe setup\". O detalhe está no fim do arquivo {install_log}."
        )),
    }
    match &o.cable {
        Cable::Present => done.push("VB-Cable (microfone virtual): já estava instalado.".into()),
        Cable::Installed => done.push("VB-Cable (microfone virtual): instalado.".into()),
        // the installer closing says nothing about whether "Install Driver" was clicked: both ways out are named
        Cable::NeedsRestart => todo.push(
            "VB-Cable: o instalador fechou, mas o microfone virtual ainda não apareceu. \
             Se você concluiu a instalação, reinicie o computador; se não, rode \"clarim.exe setup\" de novo e clique em \"Install Driver\"."
                .into(),
        ),
        Cable::Changed => todo.push(format!(
            "VB-Cable: o pacote no site da VB-Audio mudou e, por segurança, o Clarim não o executou. \
             Instale o VB-Cable em {VBCABLE_PAGE}."
        )),
        Cable::Failed(msg) => todo.push(format!("VB-Cable: {} Você pode instalá-lo em {VBCABLE_PAGE}.", sentence(msg))),
    }
    match &o.apo {
        Apo::Present => done.push("Equalizer APO (equalizador de saída): já estava instalado.".into()),
        Apo::Installed => done.push("Equalizer APO (equalizador de saída): instalado.".into()),
        // the service's own sentence, which already names the Equalizer APO ("Equalizer APO instalado, mas um aparelho...")
        Apo::Warning(msg) => todo.push(sentence(msg)),
        Apo::Failed(msg) => todo.push(format!(
            "Equalizer APO: {} Depois, tente de novo no painel, no cartão \"Equalizador de saída\".",
            sentence(msg)
        )),
        Apo::Skipped => todo.push("Equalizer APO: não instalado, porque o serviço do Clarim não está funcionando.".into()),
    }
    match &o.audio {
        Audio::NotNeeded => {}
        Audio::Restarted => done.push("Áudio do Windows reiniciado para ligar o equalizador (o som pode ter piscado).".into()),
        Audio::Failed(msg) => {
            todo.push(format!("{} Se o equalizador não funcionar, reinicie o computador.", sentence(msg)));
        }
    }

    let headline = if !o.service.ok() {
        "Não foi possível instalar o Clarim."
    } else if todo.is_empty() {
        "Tudo pronto! O Clarim está instalado."
    } else {
        "O Clarim foi instalado, mas ficou algo pendente."
    };
    // Nothing here promises that a window opens: whether the dashboard follows depends on who started this process (the
    // launcher opens it, an administrator shell does not), and its first-steps card on the saved config.
    let closing = if o.service == Service::Stuck {
        "Feche este aviso e siga o passo em Pendente. O arquivo acima mostra o erro."
    } else if !o.service.ok() {
        "Feche este aviso e tente de novo (abra o Clarim outra vez, ou rode \"clarim.exe setup\"). Se continuar falhando, o arquivo acima mostra o erro."
    } else if todo.is_empty() {
        "Falta só uma coisa: no Discord, Meet ou Zoom, escolha \"CABLE Output\" como microfone \
         (e desligue o controle automático de ganho do app).\n\n\
         O painel do Clarim abre pelo ícone do Clarim na bandeja ou pelo menu Iniciar."
    } else {
        "Quando resolver o que está pendente, escolha \"CABLE Output\" como microfone no Discord, Meet ou Zoom \
         (e desligue o controle automático de ganho do app). O painel do Clarim abre pelo ícone do Clarim na bandeja ou pelo menu Iniciar."
    };
    let list = |title: &str, items: &[String]| -> String {
        if items.is_empty() {
            String::new()
        } else {
            format!("{title}\n{}\n\n", items.iter().map(|i| format!("  - {i}")).collect::<Vec<_>>().join("\n"))
        }
    };
    Summary {
        text: format!("{headline}\n\n{}{}{closing}", list("Feito:", &done), list("Pendente:", &todo)),
        warn: !todo.is_empty(),
    }
}

// ------------------------------------------------------------------------------------------ waiting on the service

/// Asks `poll` for the service's status every `step` until `ready` accepts one. More than `max_errors` failures in a row
/// end it with the last error (a service that is still starting fails for a while, hence `u32::MAX` there); the timeout
/// ends it too.
fn wait_until(
    mut poll: impl FnMut() -> Result<ApoServiceStatus>,
    ready: impl Fn(&ApoServiceStatus) -> bool,
    timeout: Duration,
    step: Duration,
    max_errors: u32,
) -> Result<ApoServiceStatus> {
    let deadline = Instant::now() + timeout;
    let (mut errors, mut said) = (0u32, None::<String>);
    loop {
        match poll() {
            Ok(status) if ready(&status) => return Ok(status),
            Ok(status) => {
                errors = 0;
                if status.busy != said {
                    log::info!("setup: the service is working: {}", status.busy.as_deref().unwrap_or("-"));
                    said = status.busy;
                }
            }
            Err(e) => {
                errors = errors.saturating_add(1);
                if errors > max_errors {
                    return Err(e);
                }
                log::debug!("setup: the service did not answer yet: {e:#}");
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("O serviço do Clarim demorou demais para responder."));
        }
        std::thread::sleep(step);
    }
}

// ------------------------------------------------------------------------------------------ the steps

/// Every active playback device (COM stays off this thread, as in the agent).
fn playback_devices() -> Result<Vec<DeviceInfo>> {
    let scan = std::thread::spawn(backend::scan_devices).join().map_err(|_| anyhow!("the device scan panicked"))??;
    Ok(scan.outputs)
}

fn wait_for_cable(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if playback_devices().is_ok_and(|outputs| has_cable_input(&outputs)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// VB-Cable, if "CABLE Input" is not there yet: download and check VB-Audio's own package, tell the user what to
/// click, show VB-Audio's own installer and wait for it, then look for the device again.
fn cable_step() -> Cable {
    match playback_devices() {
        Ok(outputs) if has_cable_input(&outputs) => {
            log::info!("setup: VB-Cable is already installed (CABLE Input found)");
            return Cable::Present;
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("setup: cannot list the playback devices: {e:#}");
            return Cable::Failed("Não foi possível ver se o VB-Cable já está instalado.".into());
        }
    }
    let installer = match vbcable::prepare() {
        Ok(Package::Ready(path)) => path,
        Ok(Package::Changed) => {
            log::warn!("setup: the VB-Cable package changed on VB-Audio's site; nothing was run");
            return Cable::Changed;
        }
        Err(e) => {
            log::warn!("setup: VB-Cable download failed: {e:#}");
            return Cable::Failed(user_text(&e, "Não foi possível baixar o VB-Cable."));
        }
    };
    tell(VBCABLE_INTRO_TEXT, false);
    log::info!("setup: starting VB-Audio's own installer {}", installer.display());
    if let Err(e) = vbcable::run_setup(&installer) {
        log::warn!("setup: the VB-Cable installer: {e:#}");
        return Cable::Failed(user_text(&e, "Não foi possível abrir o instalador do VB-Cable."));
    }
    if wait_for_cable(CABLE_WAIT) {
        log::info!("setup: VB-Cable installed (CABLE Input found)");
        Cable::Installed
    } else {
        log::warn!("setup: the VB-Cable installer closed but CABLE Input is not there yet");
        Cable::NeedsRestart
    }
}

/// The endpoint GUIDs the dashboard's install button would attach Equalizer APO to (the physical devices the four channels
/// play on). On a first run the agent may not have saved its config yet: apply the same first-run defaults it will.
fn apo_targets(outputs: &[DeviceInfo]) -> Vec<String> {
    let mut cfg = Config::load();
    if !Config::path().exists() {
        backend::first_run_defaults(&mut cfg, outputs);
    }
    eqapo::install_targets(&cfg, outputs)
}

/// Equalizer APO, if the service does not report it installed: one `apo_install` job in the service, followed to its end.
/// Then Windows Audio restarts once when the service says the change needs it.
fn apo_step() -> (Apo, Audio) {
    let first = match wait_until(clarim_svc::apo_status, |_| true, SERVICE_WAIT, POLL, u32::MAX) {
        Ok(status) => status,
        Err(e) => {
            log::error!("setup: the service does not answer: {e:#}");
            return (Apo::Failed(e.to_string()), Audio::NotNeeded);
        }
    };
    if first.installed {
        log::info!("setup: Equalizer APO is already installed; left alone");
        return (Apo::Present, Audio::NotNeeded);
    }
    let targets = playback_devices().map(|outputs| apo_targets(&outputs)).unwrap_or_else(|e| {
        log::warn!("setup: cannot list the playback devices ({e:#}); Equalizer APO is installed without attaching any");
        Vec::new()
    });
    log::info!("setup: asking the service to install Equalizer APO and attach {} device(s)", targets.len());
    if let Err(e) = clarim_svc::apo_install(&targets) {
        log::warn!("setup: the service refused the Equalizer APO install: {e:#}");
        return (Apo::Failed(e.to_string()), Audio::NotNeeded);
    }
    let after = match wait_until(clarim_svc::apo_status, |s| s.busy.is_none(), APO_WAIT, POLL, 5) {
        Ok(status) => status,
        Err(e) => {
            log::error!("setup: lost track of the Equalizer APO install: {e:#}");
            return (Apo::Failed(e.to_string()), Audio::NotNeeded);
        }
    };
    let apo = apo_outcome(&after);
    log::info!("setup: Equalizer APO: {apo:?}");
    let audio = if needs_audio_restart(true, &after) { restart_audio() } else { Audio::NotNeeded };
    (apo, audio)
}

fn restart_audio() -> Audio {
    log::info!("setup: restarting Windows Audio so the equalizer takes effect");
    if let Err(e) = clarim_svc::apo_restart_audio() {
        return Audio::Failed(e.to_string());
    }
    match wait_until(clarim_svc::apo_status, |s| s.busy.is_none(), RESTART_WAIT, POLL, 5) {
        Ok(status) => status.error.map_or(Audio::Restarted, Audio::Failed),
        Err(e) => Audio::Failed(e.to_string()),
    }
}

/// The Clarim service, only when it is missing. One that is installed is left as it is (`clarim_svc::install` would stop the
/// running service and its agent, and could put an older downloaded exe over a newer installed one), and started if it is
/// stopped. A first install that fails after the service was registered is undone, so the half-made service does not make
/// the next run (and `should_offer`) believe Clarim is installed. The four calls are parameters so the decisions can be tested.
fn service_step_with(
    is_installed: impl Fn() -> bool,
    start: impl FnOnce() -> Result<()>,
    install: impl FnOnce() -> Result<()>,
    uninstall: impl FnOnce() -> Result<()>,
) -> Service {
    if is_installed() {
        return match start() {
            Ok(()) => {
                log::info!("setup: the Clarim service is already installed; left alone");
                Service::Present
            }
            Err(e) => {
                log::error!("setup: the installed Clarim service cannot be started: {e:#}");
                Service::Stuck
            }
        };
    }
    let Err(e) = install() else { return Service::Installed };
    log::error!("setup: the service install failed: {e:#}");
    if !is_installed() {
        return Service::Failed;
    }
    log::warn!("setup: the service was registered before the failure; undoing the install");
    match uninstall() {
        Ok(()) => Service::Failed,
        Err(e) => {
            log::error!("setup: could not undo the install: {e:#}");
            Service::Stuck
        }
    }
}

/// The three steps, in order, each logged (to `install.log`); a step that fails does not stop the next one that can still run.
/// VB-Cable goes first on purpose: the service starts the agent right away, and the agent decides its first-run defaults
/// (channels moved off a virtual default playback device, which a new "CABLE Input" may become) from the devices it sees
/// at that moment, once.
fn perform() -> Outcome {
    log::info!("setup 1/3: VB-Cable");
    let cable = cable_step();
    log::info!("setup 2/3: the Clarim service");
    let service = service_step_with(clarim_svc::is_installed, clarim_svc::start_installed, clarim_svc::install, clarim_svc::uninstall);
    log::info!("setup 3/3: Equalizer APO");
    let (apo, audio) = if service.ok() { apo_step() } else { (Apo::Skipped, Audio::NotNeeded) };
    Outcome { service, cable, apo, audio }
}

// ------------------------------------------------------------------------------------------ windows: dialogs, elevation

fn message_box(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let (text, title) = (open::wide(OsStr::new(text)), open::wide(OsStr::new(TITLE)));
    // SAFETY: both strings are NUL-terminated and outlive the call; no owner window.
    unsafe { MessageBoxW(HWND::default(), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), style | MB_SETFOREGROUND | MB_TOPMOST) }
}

fn ask_yes_no(text: &str) -> bool {
    message_box(text, MB_YESNO | MB_ICONQUESTION) == IDYES
}

fn tell(text: &str, warn: bool) {
    message_box(text, MB_OK | if warn { MB_ICONWARNING } else { MB_ICONINFORMATION });
}

fn is_elevated() -> bool {
    // SAFETY: no arguments, no pointers.
    unsafe { IsUserAnAdmin() }.as_bool()
}

enum Relaunch {
    /// The user said No at the UAC prompt.
    Declined,
    /// The elevated process ran and ended with this exit code.
    Exited(u32),
    /// The elevated process started, but its end could not be followed: no process handle, no end within `SETUP_WAIT_MS`, no
    /// exit code.
    Unknown,
}

/// Starts this exe again with `args`, asking for administrator rights (UAC), and waits for it to end (`SETUP_WAIT_MS` at most).
/// An `Err` is Windows refusing to start it, and the user has been told (`UAC_FAILED_TEXT`).
fn relaunch_elevated(args: &str) -> Result<Relaunch> {
    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    let (file, params) = (open::wide(exe.as_os_str()), open::wide(OsStr::new(args)));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC: without COM on this thread ShellExecuteEx may finish on a worker thread, and the process handle
        // would not be there yet when it returns
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialised with its size; the strings it points to outlive the call.
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        if e.code() == ERROR_CANCELLED.to_hresult() {
            return Ok(Relaunch::Declined);
        }
        log::error!("setup: cannot start the elevated copy: {e:#}");
        tell(UAC_FAILED_TEXT, true); // a GUI-subsystem exe has no console to say it on
        return Err(anyhow::Error::new(e).context("cannot ask Windows for administrator rights"));
    }
    if info.hProcess.is_invalid() {
        log::error!("setup: Windows started the elevated copy without giving a process to wait for");
        return Ok(Relaunch::Unknown);
    }
    // SAFETY: with SEE_MASK_NOCLOSEPROCESS a successful call hands us the process handle, which only we own now.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    let handle = HANDLE(process.as_raw_handle());
    let mut code = 1u32;
    // SAFETY: `handle` stays open until `process` drops at the end of the function; `code` is a valid out pointer.
    let ended = unsafe {
        let waited = WaitForSingleObject(handle, SETUP_WAIT_MS);
        if waited == WAIT_OBJECT_0 {
            GetExitCodeProcess(handle, &mut code).map_err(|e| anyhow!("cannot read the exit code: {e}"))
        } else {
            Err(anyhow!("no end after {} min (wait result {})", SETUP_WAIT_MS / 60_000, waited.0))
        }
    };
    match ended {
        Ok(()) => Ok(Relaunch::Exited(code)),
        Err(e) => {
            log::error!("setup: cannot follow the elevated copy to its end: {e:#}");
            Ok(Relaunch::Unknown)
        }
    }
}

// ------------------------------------------------------------------------------------------ entry points

fn dashboard_listening(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(400)).is_ok()
}

/// After the install: VB-Cable's page when the elevated setup asked for it, then the dashboard once the agent (started
/// by the service in this user session) answers.
fn follow_up(code: u32) {
    if code == u32::from(EXIT_VBCABLE_MANUAL) {
        if let Err(e) = open::shell_execute(OsStr::new(VBCABLE_PAGE), None) {
            log::warn!("cannot open {VBCABLE_PAGE}: {e:#}");
        }
    }
    if !clarim_svc::is_installed() {
        return; // the service did not get installed: there is no agent to wait for
    }
    let port = Config::load().port;
    let deadline = Instant::now() + DASHBOARD_WAIT;
    while !dashboard_listening(port) {
        if Instant::now() >= deadline {
            log::warn!("the dashboard did not come up within {} s; not opening it", DASHBOARD_WAIT.as_secs());
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Err(e) = open::open_dashboard(port) {
        log::warn!("cannot open the dashboard: {e:#}");
    }
}

/// A text for an ending the elevated copy's own summary dialog did not cover, with where the detail may be.
fn interrupted_text(what: &str) -> String {
    format!("{what}\n\nO detalhe, se houver, está no fim do arquivo {}.", clarim_svc::install_log_path().display())
}

/// 0, 1 and `EXIT_VBCABLE_MANUAL` are the endings the elevated copy tells the user about itself (its summary dialog). Anything
/// else (a crash code, a kill) it did not.
fn told_by_the_elevated_copy(code: u32) -> bool {
    matches!(code, 0 | 1) || code == u32::from(EXIT_VBCABLE_MANUAL)
}

/// The exit status of a process that ended with `code` (0 is success; a code that does not fit in a `u8` is a plain 1).
fn exit_status(code: u32) -> Result<()> {
    match u8::try_from(code) {
        Ok(0) => Ok(()),
        Ok(code) => Err(Incomplete(code).into()),
        Err(_) => Err(Incomplete(1).into()),
    }
}

/// Hands over to an elevated `setup`, waits for it and passes its exit status on; when it is done and `open` is set, opens what
/// it asks for (`follow_up`). A declined UAC prompt is a short message and a clean exit.
fn elevate_and_follow(open: bool) -> Result<()> {
    match relaunch_elevated(ELEVATED_ARGS)? {
        Relaunch::Declined => {
            log::info!("setup: administrator rights were declined");
            tell(UAC_DECLINED_TEXT, true);
            Ok(())
        }
        Relaunch::Unknown => {
            tell(&interrupted_text("O Clarim não conseguiu acompanhar a instalação até o fim. Se houver uma janela do Clarim aberta, termine o que ela pede."), true);
            Err(Incomplete(1).into())
        }
        Relaunch::Exited(code) => {
            log::info!("setup: the elevated process ended with code {code}");
            if !told_by_the_elevated_copy(code) {
                tell(&interrupted_text(&format!("A instalação terminou de forma inesperada (código {code:#x}). Tente de novo.")), true);
            } else if open {
                follow_up(code);
            }
            exit_status(code)
        }
    }
}

/// `Some(guard)` when this process may go ahead. `None` after telling the user why not: another setup is running, or the
/// slot cannot be claimed at all (not guessed to be free: two installs at once would fight over the service).
fn claim(slot: Result<Option<InstanceGuard>>) -> Option<InstanceGuard> {
    match slot {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            log::info!("setup: another setup is already running");
            tell(ALREADY_RUNNING_TEXT, false);
            None
        }
        Err(e) => {
            log::error!("setup: {e:#}");
            tell(CANNOT_START_TEXT, true);
            None
        }
    }
}

/// The work, in this (elevated) process: one at a time on the PC, the three steps, the summary dialog. Opens nothing: a window or
/// a browser started from here would run elevated too (the launcher opens the dashboard, or the user does, from the tray or the
/// Start menu, which the summary says). `log_file` is where this process's log goes, which the summary names.
fn install_elevated(log_file: &Path) -> Result<()> {
    let Some(slot) = claim(instance::acquire_setup()) else { return Err(Incomplete(1).into()) };
    let outcome = perform();
    drop(slot); // before the dialog: a summary left open must not block the next try
    let summary = summary(&outcome, &log_file.display().to_string());
    log::info!("setup summary:\n{}", summary.text);
    tell(&summary.text, summary.warn);
    exit_status(u32::from(outcome.exit_code()))
}

/// `clarim` with no arguments, with nothing of Clarim there (`should_offer`): asks, and on "Sim" runs the guided install (an
/// already elevated process runs it right here, with no second launch).
/// `Ok(true)`: it was offered and answered Yes (the install ran, or the UAC prompt was declined, or another setup is running): the
/// caller is done.
/// `Ok(false)`: "Não": the caller runs the agent in this process, as it always did.
pub fn offer() -> Result<bool> {
    let elevated = is_elevated();
    // Claimed before asking: a second double click while an install runs says so instead of offering it again.
    let _launcher = if elevated {
        None
    } else {
        match claim(instance::acquire_setup_launcher()) {
            Some(guard) => Some(guard),
            None => return Ok(true),
        }
    };
    if !ask_yes_no(OFFER_TEXT) {
        log::info!("setup: the guided install was declined; running the agent in this process");
        return Ok(false);
    }
    if elevated {
        // this process logs to clarim.log (the no-argument mode), not to install.log
        install_elevated(&crate::logging::log_path())?;
    } else {
        elevate_and_follow(true)?;
    }
    Ok(true)
}

/// `clarim setup [--no-open]`. Not elevated: ask for administrator rights and let the elevated copy do the work. Elevated:
/// do it and show the summary; the dashboard is not opened from here (see `install_elevated`), `open` or not.
pub fn run(open: bool) -> Result<()> {
    if is_elevated() {
        if open {
            log::info!("setup: running elevated, so the dashboard is not opened from here (a window started by this process would be elevated too)");
        }
        return install_elevated(&clarim_svc::install_log_path());
    }
    let Some(_launcher) = claim(instance::acquire_setup_launcher()) else { return Err(Incomplete(1).into()) };
    log::info!("setup needs administrator rights: asking Windows");
    elevate_and_follow(open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarim_core::is_virtual_device;
    use std::cell::Cell;

    fn dev(name: &str) -> DeviceInfo {
        DeviceInfo { id: format!("id-{name}"), name: name.into(), is_default: false, is_virtual: is_virtual_device(name) }
    }

    fn all_done() -> Outcome {
        Outcome { service: Service::Installed, cable: Cable::Installed, apo: Apo::Installed, audio: Audio::Restarted }
    }

    const LOG: &str = r"C:\Program Files\Clarim\install.log";

    // the launcher's wait is bounded, and by more than the elevated copy's own limits add up to (downloads, VB-Audio's
    // installer, the Equalizer APO job, the audio restart: 5 + 10 + 12 + 3 minutes)
    const _: () = assert!(SETUP_WAIT_MS > (5 + 10 + 12 + 3) * 60 * 1000 && SETUP_WAIT_MS < 60 * 60 * 1000);

    #[test]
    fn the_guided_install_is_offered_only_when_nothing_of_clarim_is_there() {
        assert!(should_offer(false, false, true));
        assert!(!should_offer(true, false, true), "the service is installed: today's behaviour");
        assert!(!should_offer(false, true, true), "an agent runs: the dashboard opens instead");
        assert!(!should_offer(false, false, false), "`clarim --dry-run` wants the agent in this process");
    }

    #[test]
    fn only_a_cable_input_device_counts_as_vb_cable() {
        let real = dev("Alto-falantes (Realtek(R) Audio)");
        assert!(!has_cable_input(&[]));
        assert!(!has_cable_input(std::slice::from_ref(&real)));
        assert!(has_cable_input(&[real.clone(), dev("CABLE Input (VB-Audio Virtual Cable)")]));
        assert!(has_cable_input(&[dev("cable input (vb-audio virtual cable)")]), "case-insensitive");
        // other virtual mics are usable by the engine, but the call apps are told to pick "CABLE Output"
        assert!(!has_cable_input(&[dev("SteelSeries Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)")]));
        assert!(!has_cable_input(&[dev("Virtual Audio Driver Output")]));
        assert!(!has_cable_input(&[dev("CABLE Output (VB-Audio Virtual Cable)")]), "that is the capture side");
    }

    fn status(installed: bool, error: Option<&str>, restart: bool, busy: Option<&str>) -> ApoServiceStatus {
        ApoServiceStatus {
            installed,
            error: error.map(str::to_string),
            needs_restart: restart,
            busy: busy.map(str::to_string),
            ..ApoServiceStatus::default()
        }
    }

    #[test]
    fn the_equalizer_install_is_judged_from_the_services_status() {
        assert_eq!(apo_outcome(&status(true, None, true, None)), Apo::Installed);
        assert_eq!(apo_outcome(&status(true, Some("um aparelho não foi configurado"), true, None)), Apo::Warning("um aparelho não foi configurado".into()));
        assert_eq!(apo_outcome(&status(false, Some("Sem internet."), false, None)), Apo::Failed("Sem internet.".into()));
        assert!(matches!(apo_outcome(&status(false, None, false, None)), Apo::Failed(m) if m.contains("não terminou")));
    }

    #[test]
    fn windows_audio_restarts_only_for_a_change_this_setup_made_and_that_needs_it() {
        assert!(needs_audio_restart(true, &status(true, None, true, None)));
        assert!(!needs_audio_restart(true, &status(true, None, false, None)), "nothing to take up");
        assert!(!needs_audio_restart(true, &status(false, Some("x"), true, None)), "a failed install has nothing to restart for");
        assert!(!needs_audio_restart(false, &status(true, None, true, None)), "an Equalizer APO that was already there is not touched");
    }

    #[test]
    fn a_complete_install_says_so_and_points_at_cable_output() {
        let s = summary(&all_done(), LOG);
        assert!(!s.warn);
        assert!(s.text.starts_with("Tudo pronto!"), "{}", s.text);
        for line in ["Feito:", "Clarim instalado como serviço", "VB-Cable (microfone virtual): instalado.", "Equalizer APO (equalizador de saída): instalado.", "Áudio do Windows reiniciado"] {
            assert!(s.text.contains(line), "{line}: {}", s.text);
        }
        assert!(!s.text.contains("Pendente:"), "{}", s.text);
        assert!(s.text.contains("\"CABLE Output\"") && s.text.contains("controle automático de ganho"), "{}", s.text);
        assert_eq!(all_done().exit_code(), 0);
    }

    #[test]
    fn what_was_already_there_is_said_so_and_a_restart_that_was_not_needed_is_not_mentioned() {
        let o = Outcome { service: Service::Present, cable: Cable::Present, apo: Apo::Present, audio: Audio::NotNeeded };
        let s = summary(&o, LOG);
        for line in ["Clarim: o serviço já estava instalado", "VB-Cable (microfone virtual): já estava instalado", "Equalizer APO (equalizador de saída): já estava instalado"] {
            assert!(s.text.contains(line), "{line}: {}", s.text);
        }
        assert!(!s.text.contains("Clarim instalado como serviço"), "nothing was installed: {}", s.text);
        assert!(!s.text.contains("reiniciado") && !s.warn);
        assert_eq!(o.exit_code(), 0);
    }

    #[test]
    fn a_vb_cable_that_needs_a_restart_is_pending_but_not_a_failure() {
        let o = Outcome { cable: Cable::NeedsRestart, ..all_done() };
        let s = summary(&o, LOG);
        assert!(s.warn && s.text.starts_with("O Clarim foi instalado, mas ficou algo pendente."), "{}", s.text);
        assert!(s.text.contains("Pendente:") && s.text.contains("Se você concluiu a instalação, reinicie o computador; se não, rode \"clarim.exe setup\" de novo"), "{}", s.text);
        assert!(!s.text.contains("Reinicie o computador para terminar"), "a closed installer is not proof that it finished: {}", s.text);
        assert!(s.text.contains("Feito:") && s.text.contains("Equalizer APO (equalizador de saída): instalado."), "what worked is still listed");
        assert_eq!(o.exit_code(), 0);
    }

    #[test]
    fn a_changed_package_runs_nothing_and_sends_the_user_to_vb_audios_page() {
        let o = Outcome { cable: Cable::Changed, ..all_done() };
        let s = summary(&o, LOG);
        assert!(s.warn && s.text.contains("não o executou") && s.text.contains("https://vb-audio.com/Cable/"), "{}", s.text);
        assert!(!s.text.contains("vai abrir"), "an elevated shell opens nothing, so the text must not promise a page: {}", s.text);
        assert_eq!(o.exit_code(), EXIT_VBCABLE_MANUAL, "the launcher opens the page in the user's own browser");
        assert_ne!(EXIT_VBCABLE_MANUAL, crate::agent::EXIT_ALREADY_RUNNING);
    }

    #[test]
    fn failures_name_the_reason_and_what_to_do_next() {
        let o = Outcome {
            service: Service::Installed,
            cable: Cable::Failed("Não foi possível baixar o VB-Cable. Verifique a conexão com a internet e tente de novo.".into()),
            apo: Apo::Failed("Sem internet".into()),
            audio: Audio::Failed("O áudio voltou, mas estes serviços não: X".into()),
        };
        let s = summary(&o, LOG);
        assert!(s.warn);
        assert!(s.text.contains("VB-Cable: Não foi possível baixar o VB-Cable. Verifique a conexão com a internet e tente de novo. Você pode instalá-lo em https://vb-audio.com/Cable/."), "{}", s.text);
        assert!(s.text.contains("Equalizer APO: Sem internet. Depois, tente de novo no painel"), "{}", s.text);
        assert!(s.text.contains("X. Se o equalizador não funcionar, reinicie o computador."), "{}", s.text);
        assert_eq!(o.exit_code(), 1);
        let said = "Equalizer APO instalado, mas um aparelho não foi configurado: Este aparelho não aceita o equalizador.";
        let warned = Outcome { apo: Apo::Warning(said.into()), cable: Cable::Present, audio: Audio::NotNeeded, ..all_done() };
        assert!(summary(&warned, LOG).text.contains(&format!("  - {said}\n")), "{}", summary(&warned, LOG).text);
        assert_eq!(warned.exit_code(), 0, "installed with a warning is not a failure");
    }

    #[test]
    fn without_the_service_nothing_else_pretends_to_work() {
        let o = Outcome { service: Service::Failed, cable: Cable::Installed, apo: Apo::Skipped, audio: Audio::NotNeeded };
        let s = summary(&o, LOG);
        assert!(s.warn && s.text.starts_with("Não foi possível instalar o Clarim."), "{}", s.text);
        assert!(s.text.contains(LOG) && s.text.contains("tente de novo (abra o Clarim outra vez, ou rode \"clarim.exe setup\")"), "{}", s.text);
        assert!(s.text.contains("não instalado, porque o serviço do Clarim não está funcionando"), "{}", s.text);
        assert!(!s.text.contains("Tudo pronto") && !s.text.contains("Falta só uma coisa"), "{}", s.text);
        assert_eq!(o.exit_code(), 1);
    }

    #[test]
    fn a_service_that_could_not_be_undone_sends_the_user_to_uninstall_and_never_promises_a_retry_that_would_skip_it() {
        let o = Outcome { service: Service::Stuck, cable: Cable::Present, apo: Apo::Skipped, audio: Audio::NotNeeded };
        let s = summary(&o, LOG);
        assert!(s.warn && s.text.starts_with("Não foi possível instalar o Clarim."), "{}", s.text);
        assert!(s.text.contains("\"clarim.exe uninstall\" e depois \"clarim.exe setup\"") && s.text.contains(LOG), "{}", s.text);
        // a plain "try again" would find the service installed and leave it as it is
        assert!(!s.text.contains("tente de novo") && !s.text.contains("abra o Clarim outra vez"), "{}", s.text);
        assert_eq!(o.exit_code(), 1);
    }

    /// The service decisions with the four calls faked: nothing here touches the real service.
    #[test]
    fn the_service_is_installed_only_when_missing_and_a_failed_first_install_is_undone() {
        let calls = std::cell::RefCell::new(Vec::<&str>::new());
        let run = |was: bool, after_failure: bool, start: bool, install: bool, uninstall: bool| {
            calls.borrow_mut().clear();
            let state = Cell::new(was);
            let outcome = service_step_with(
                || state.get(),
                || {
                    calls.borrow_mut().push("start");
                    if start { Ok(()) } else { Err(anyhow!("cannot start")) }
                },
                || {
                    calls.borrow_mut().push("install");
                    state.set(after_failure); // what `is_installed` says once the install ended
                    if install { Ok(()) } else { Err(anyhow!("cannot create")) }
                },
                || {
                    calls.borrow_mut().push("uninstall");
                    if uninstall { Ok(()) } else { Err(anyhow!("cannot delete")) }
                },
            );
            (outcome, calls.borrow().clone())
        };
        // already there: never installed over, only started
        assert_eq!(run(true, true, true, true, true), (Service::Present, vec!["start"]));
        assert_eq!(run(true, true, false, true, true), (Service::Stuck, vec!["start"]), "one that will not start is not a success");
        // missing: installed, and nothing to undo when it works
        assert_eq!(run(false, true, true, true, true), (Service::Installed, vec!["install"]));
        // a failure before the service was registered leaves nothing to undo
        assert_eq!(run(false, false, true, false, true), (Service::Failed, vec!["install"]));
        // a failure after it was registered: undone, so the next run (and the offer) starts clean
        assert_eq!(run(false, true, true, false, true), (Service::Failed, vec!["install", "uninstall"]));
        // ... and when that cannot be undone either, it is said, not hidden
        assert_eq!(run(false, true, true, false, false), (Service::Stuck, vec!["install", "uninstall"]));
    }

    #[test]
    fn the_launcher_tells_a_crash_from_a_summary_it_already_saw_and_passes_the_exit_status_on() {
        for told in [0, 1, u32::from(EXIT_VBCABLE_MANUAL)] {
            assert!(told_by_the_elevated_copy(told), "{told}");
        }
        for not_told in [2, 3, 259, 0xC000_0005, 0xC000_013A] {
            assert!(!told_by_the_elevated_copy(not_told), "{not_told:#x}");
        }
        let code = |r: Result<()>| r.unwrap_err().downcast_ref::<Incomplete>().map(|i| i.0);
        assert!(exit_status(0).is_ok());
        assert_eq!(code(exit_status(1)), Some(1));
        assert_eq!(code(exit_status(u32::from(EXIT_VBCABLE_MANUAL))), Some(EXIT_VBCABLE_MANUAL));
        assert_eq!(code(exit_status(0xC000_0005)), Some(1), "a code that does not fit in a byte is a plain failure");
        let text = interrupted_text("A instalação terminou de forma inesperada.");
        assert!(text.contains("inesperada") && text.contains("install.log"), "{text}");
    }

    #[test]
    fn sentences_get_one_full_stop() {
        assert_eq!(sentence("Sem internet"), "Sem internet.");
        assert_eq!(sentence("  Sem internet.  "), "Sem internet.");
        assert_eq!(sentence("Deu certo!"), "Deu certo!");
    }

    #[test]
    fn the_dialogs_say_what_the_task_asks() {
        for needle in [
            "Instalar o Clarim neste computador? (recomendado)",
            "serviço do Clarim",
            "VB-Cable (microfone virtual, da VB-Audio)",
            "Equalizer APO",
            "só o que ainda não estiver no PC",
            "o áudio do Windows pode reiniciar (o som cai por alguns segundos)",
            "permissão de administrador uma vez",
        ] {
            assert!(OFFER_TEXT.contains(needle), "{needle}");
        }
        for needle in ["Agora vai abrir o instalador do VB-Cable, da VB-Audio.", "\"Install Driver\"", "doação (donationware)", "vb-cable.com"] {
            assert!(VBCABLE_INTRO_TEXT.contains(needle), "{needle}");
        }
        assert!(UAC_DECLINED_TEXT.contains("cancelada") && UAC_DECLINED_TEXT.contains("clique em Sim") && UAC_DECLINED_TEXT.contains("clarim.exe setup"));
        assert!(UAC_FAILED_TEXT.contains("Executar como administrador"));
        assert!(ALREADY_RUNNING_TEXT.contains("já está em andamento"));
    }

    #[test]
    fn the_elevated_copy_is_started_with_arguments_the_parser_accepts() {
        let args: Vec<String> = ELEVATED_ARGS.split(' ').map(str::to_string).collect();
        assert_eq!(crate::parse(&args).unwrap(), crate::Command::Setup { open: false });
    }

    #[test]
    fn waiting_for_the_service_follows_its_busy_text_and_gives_up_on_errors_and_on_time() {
        let step = Duration::from_millis(1);
        let long = Duration::from_secs(5);
        // busy twice, then idle
        let calls = Cell::new(0);
        let idle = wait_until(
            || {
                calls.set(calls.get() + 1);
                Ok(status(true, None, false, (calls.get() < 3).then_some("Instalando o Equalizer APO...")))
            },
            |s| s.busy.is_none(),
            long,
            step,
            5,
        )
        .unwrap();
        assert_eq!((calls.get(), idle.installed), (3, true));
        // a service that is still starting fails for a while, then answers
        let calls = Cell::new(0);
        let up = wait_until(
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 8 { Err(anyhow!("O serviço do Clarim não respondeu a tempo.")) } else { Ok(status(false, None, false, None)) }
            },
            |_| true,
            long,
            step,
            u32::MAX,
        )
        .unwrap();
        assert_eq!((calls.get(), up.installed), (8, false));
        // too many errors in a row: the last one comes back; one that was followed by an answer does not count
        let calls = Cell::new(0);
        let err = wait_until(
            || {
                calls.set(calls.get() + 1);
                Err(anyhow!("Falha na comunicação com o serviço do Clarim."))
            },
            |_| true,
            long,
            step,
            5,
        )
        .unwrap_err();
        assert_eq!((calls.get(), err.to_string().as_str()), (6, "Falha na comunicação com o serviço do Clarim."));
        let calls = Cell::new(0);
        let ok = wait_until(
            || {
                calls.set(calls.get() + 1);
                match calls.get() {
                    1..=4 | 6..=9 => Err(anyhow!("hiccup")),
                    5 => Ok(status(true, None, false, Some("busy"))),
                    _ => Ok(status(true, None, false, None)),
                }
            },
            |s| s.busy.is_none(),
            long,
            step,
            5,
        );
        assert!(ok.is_ok(), "an answer in between resets the count");
        // busy for ever: the timeout ends it
        let err = wait_until(|| Ok(status(true, None, false, Some("busy"))), |s| s.busy.is_none(), Duration::from_millis(20), step, 5).unwrap_err();
        assert!(err.to_string().contains("demorou demais"), "{err}");
    }
}
