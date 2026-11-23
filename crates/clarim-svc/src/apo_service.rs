//! The Equalizer APO side of the Clarim service: what the dashboard can ask for, one operation at a time.
//!
//! - `status`: is APO installed, what each playback endpoint looks like, is an operation running, does
//!   Windows Audio need a restart for the registry to take effect.
//! - `set_device`: attach / detach APO on one endpoint (a few registry writes, done inline).
//! - `start_install`: download + check + run the pinned installer, then attach the requested devices (a
//!   minute or so, on a worker thread).
//! - `start_restart`: restart Windows Audio and its dependents (seconds, on a worker thread).
//!
//! Everything that touches the system goes through `Platform`, so the sequencing, the validation and the
//! status rules are tested with a fake. Every input is validated here: the pipe is a trust boundary (any
//! interactive user can connect), so a request can only name endpoint GUIDs that exist, never a path or a
//! command.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clarim_core::{is_braced_guid, is_virtual_device, EqApoDevice};
use serde::{Deserialize, Serialize};

use crate::apo_plan::{classify, plan_install, plan_restore, plan_uninstall, Attach, Plan};
use crate::apo_reg::EndpointInfo;
use crate::audio_restart::{restart_allowed, MSG_TOO_SOON};
use crate::audiodg::AudioDg;
use crate::user_error::{user_err, user_text};

pub(crate) const MAX_INSTALL_DEVICES: usize = 16;
const MAX_STATUS_DEVICES: usize = 100;

const BUSY_DOWNLOAD: &str = "Baixando o Equalizer APO...";
const BUSY_INSTALL: &str = "Instalando o Equalizer APO...";
const BUSY_DEVICES: &str = "Configurando os aparelhos...";
const BUSY_DEVICE: &str = "Configurando o aparelho...";
const BUSY_RESTART: &str = "Reiniciando o áudio do Windows...";

const MSG_NOT_INSTALLED: &str = "O Equalizer APO não está instalado.";
const MSG_VIRTUAL: &str = "Este aparelho não aceita o equalizador (é um dispositivo virtual ou não está presente).";
const MSG_BAD_REQUEST: &str = "Pedido inválido.";
const MSG_STOPPING: &str = "O serviço do Clarim está sendo parado. Tente de novo depois que ele voltar.";

/// What the service reports to the agent (`Status` on the pipe).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct ApoServiceStatus {
    /// A complete install: `<Program Files>\EqualizerAPO\EqualizerAPO.dll`, `HKLM\SOFTWARE\EqualizerAPO`
    /// and both classes registered. A DLL left behind by an interrupted install counts as not installed.
    pub installed: bool,
    /// PT-BR text while an install or a restart runs.
    pub busy: Option<String>,
    /// PT-BR text of how the last install or restart failed (cleared when the next one starts).
    pub error: Option<String>,
    /// Windows Audio must restart for the last device change to take effect.
    pub needs_restart: bool,
    /// An `audiodg.exe` has `EqualizerAPO.dll` loaded (`None` = cannot tell).
    pub apo_loaded: Option<bool>,
    /// Every playback endpoint that is present.
    pub devices: Vec<EqApoDevice>,
}

/// The system, as `ApoService` uses it.
pub(crate) trait Platform: Send + Sync + 'static {
    /// A complete install: files, registry key and both classes registered (not just the DLL). When false,
    /// attaching is refused and an install request runs the installer.
    fn apo_installed(&self) -> bool;
    fn list_devices(&self) -> Result<Vec<EndpointInfo>>;
    /// One endpoint by GUID, validated (braced GUID that names an existing endpoint).
    fn read_device(&self, guid: &str) -> Result<EndpointInfo>;
    fn backup_device(&self, guid: &str) -> Result<()>;
    /// Runs `plan` on an endpoint (`guid` as returned in `EndpointInfo::guid`).
    fn apply_plan(&self, guid: &str, plan: &Plan) -> Result<()>;
    /// `DisableProtectedAudioDG` = 1; returns whether it had to be written.
    fn ensure_protected_audio_dg(&self) -> Result<bool>;
    fn download_installer(&self, cancel: &AtomicBool) -> Result<PathBuf>;
    fn verify_installer(&self, path: &Path) -> Result<()>;
    fn run_installer(&self, path: &Path, cancel: &AtomicBool) -> Result<()>;
    fn check_installed(&self) -> Result<()>;
    /// After the installer ran (or failed): delete it, turn off APO's update check.
    fn finish_install(&self, installer: &Path);
    /// Restarts Windows Audio; `Ok` lists the dependent services that did not come back.
    fn restart_audio(&self, progress: &mut dyn FnMut(&str)) -> Result<Vec<String>>;
    fn audiodg(&self) -> AudioDg;
    /// Takes Equalizer APO's stock `config.txt` (-6 dB, bass boost) out of the way once Clarim manages APO. Best
    /// effort: it logs its own failures and never fails the attach or install that called it.
    fn neutralize_stock_config(&self) {}
    /// Now, in FILETIME ticks (the unit of the registry's last-write times and of process start times).
    fn now(&self) -> u64;
}

/// The real system.
pub(crate) struct System;

/// FILETIME ticks (100 ns since 1601) of a `SystemTime`.
pub(crate) fn filetime_of(t: SystemTime) -> u64 {
    const UNIX_TO_FILETIME_SECS: u64 = 11_644_473_600;
    t.duration_since(UNIX_EPOCH).map_or(0, |d| (d.as_secs() + UNIX_TO_FILETIME_SECS) * 10_000_000 + u64::from(d.subsec_nanos()) / 100)
}

impl Platform for System {
    fn apo_installed(&self) -> bool {
        crate::apo_installer::apo_ready()
    }
    fn list_devices(&self) -> Result<Vec<EndpointInfo>> {
        crate::apo_reg::list_devices()
    }
    fn read_device(&self, guid: &str) -> Result<EndpointInfo> {
        crate::apo_reg::read_device(guid)
    }
    fn backup_device(&self, guid: &str) -> Result<()> {
        crate::apo_reg::backup_device(&crate::install_dir().join("backup"), guid)
    }
    fn apply_plan(&self, guid: &str, plan: &Plan) -> Result<()> {
        crate::apo_reg::apply_plan(guid, plan)
    }
    fn ensure_protected_audio_dg(&self) -> Result<bool> {
        crate::apo_reg::ensure_protected_audio_dg()
    }
    fn download_installer(&self, cancel: &AtomicBool) -> Result<PathBuf> {
        crate::apo_installer::download(cancel)
    }
    fn verify_installer(&self, path: &Path) -> Result<()> {
        crate::apo_installer::verify(path)
    }
    fn run_installer(&self, path: &Path, cancel: &AtomicBool) -> Result<()> {
        crate::apo_installer::run_installer(path, cancel)
    }
    fn check_installed(&self) -> Result<()> {
        crate::apo_installer::check_installed()
    }
    fn finish_install(&self, installer: &Path) {
        crate::apo_installer::cleanup(installer);
        if crate::apo_installer::apo_installed() {
            crate::apo_installer::disable_update_checker();
        }
    }
    fn restart_audio(&self, progress: &mut dyn FnMut(&str)) -> Result<Vec<String>> {
        crate::audio_restart::restart_audio(&crate::audio_restart::Scm, |m| progress(m))
    }
    fn audiodg(&self) -> AudioDg {
        crate::audiodg::inspect()
    }
    fn neutralize_stock_config(&self) {
        crate::eqapo::neutralize_stock_config();
    }
    fn now(&self) -> u64 {
        filetime_of(SystemTime::now())
    }
}

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

/// Clarim may attach APO here: the endpoint is present and is not a virtual device (cable, Sonar,
/// Voicemeeter...). The agent filters the same way; this is the service's own check.
pub(crate) fn can_configure(info: &EndpointInfo) -> bool {
    info.present() && !is_virtual_device(&info.name)
}

/// Does a running audio graph predate the last change to an endpoint's effects?
/// `latest_change`: the newest registry change that matters (FILETIME ticks); `starts`: when each running
/// `audiodg.exe` started, `None` when that could not be read, in which case only `dirty` (a change this
/// service made and no restart has followed) can say. An `audiodg.exe` that started after the change
/// built its graph from the new registry; no `audiodg.exe` at all means the next one will.
pub(crate) fn restart_needed(latest_change: Option<u64>, starts: Option<&[u64]>, dirty: bool) -> bool {
    match (latest_change, starts) {
        (Some(change), Some(starts)) => starts.iter().any(|&started| started <= change),
        (None, Some(_)) => false,
        (_, None) => dirty,
    }
}

/// An install request's device list: at most `MAX_INSTALL_DEVICES`, each a braced GUID, duplicates
/// dropped (case-insensitively), order kept.
pub(crate) fn clean_guids(guids: &[String]) -> Result<Vec<String>> {
    if guids.len() > MAX_INSTALL_DEVICES {
        return Err(user_err(MSG_BAD_REQUEST));
    }
    let mut out: Vec<String> = Vec::new();
    for g in guids {
        if !is_braced_guid(g) {
            return Err(user_err(MSG_BAD_REQUEST));
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(g)) {
            out.push(g.clone());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    busy: Option<String>,
    error: Option<String>,
    /// When this service last changed an endpoint, until a newer `audiodg.exe` or a restart took it up.
    dirty_since: Option<u64>,
    last_restart_end: Option<Instant>,
}

/// What an operation says when it ends: `Some(text)` is a warning to keep in `error` although it worked.
type Outcome = Result<Option<String>>;

pub(crate) struct ApoService<P: Platform> {
    platform: P,
    inner: Mutex<Inner>,
    cancel: AtomicBool,
}

impl<P: Platform> ApoService<P> {
    pub(crate) fn new(platform: P) -> Arc<Self> {
        Arc::new(Self { platform, inner: Mutex::new(Inner::default()), cancel: AtomicBool::new(false) })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The service is stopping: running operations give up at their next wait (an installer is killed,
    /// with the windows it opened) and no new operation starts.
    pub(crate) fn shutdown(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// After `shutdown`: waits at most `timeout` for the running operation to end, so the process does not
    /// exit between the stop and the start of Windows Audio, or leave the installer behind. True when idle.
    /// `begin` refuses once `shutdown` ran (it checks under the same lock), so nothing starts after this.
    pub(crate) fn wait_until_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.lock().busy.is_none() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Takes the one-operation-at-a-time slot, or says what is running.
    fn begin(&self, text: &str) -> Result<()> {
        let mut inner = self.lock();
        if self.cancel.load(Ordering::Relaxed) {
            return Err(user_err(MSG_STOPPING));
        }
        if let Some(running) = &inner.busy {
            return Err(user_err(format!("Já há uma operação em andamento: {running}")));
        }
        inner.busy = Some(text.to_string());
        inner.error = None;
        Ok(())
    }

    fn set_busy(&self, text: &str) {
        self.lock().busy = Some(text.to_string());
    }

    fn end(&self, outcome: Outcome) {
        let mut inner = self.lock();
        inner.busy = None;
        inner.error = match outcome {
            Ok(warning) => warning,
            Err(e) => Some(user_text(&e, "A operação falhou. Veja o registro do Clarim.")),
        };
    }

    fn mark_dirty(&self) {
        let now = self.platform.now();
        self.lock().dirty_since = Some(now);
    }

    // ---- status ----

    pub(crate) fn status(&self) -> ApoServiceStatus {
        let infos = self.platform.list_devices().unwrap_or_else(|e| {
            log::debug!("apo: cannot list the playback devices: {e:#}");
            Vec::new()
        });
        let audiodg = self.platform.audiodg();
        let configured = |i: &EndpointInfo| classify(&i.state) != Attach::Free;
        let mut inner = self.lock();
        let latest = inner.dirty_since.into_iter().chain(infos.iter().filter(|i| configured(i)).filter_map(|i| i.fx_last_write)).max();
        let needs_restart = restart_needed(latest, audiodg.starts.as_deref(), inner.dirty_since.is_some());
        if audiodg.starts.is_some() && inner.dirty_since.is_some() && !restart_needed(inner.dirty_since, audiodg.starts.as_deref(), true) {
            inner.dirty_since = None; // every audio graph running now started after our change
        }
        ApoServiceStatus {
            installed: self.platform.apo_installed(),
            busy: inner.busy.clone(),
            error: inner.error.clone(),
            needs_restart,
            apo_loaded: audiodg.apo_loaded,
            devices: infos
                .iter()
                .take(MAX_STATUS_DEVICES)
                .map(|i| EqApoDevice {
                    guid: i.guid.clone(),
                    configured: configured(i),
                    can_configure: can_configure(i),
                    enhancements_off: i.enhancements_off(),
                })
                .collect(),
        }
    }

    // ---- one device ----

    /// Attaches (`enable`) or detaches APO on one endpoint. Returns whether anything changed.
    /// Attaching needs APO installed and a real device; detaching is always allowed. On a failure half way
    /// the endpoint is put back as it was, and the first backup of it is in `<Program Files>\Clarim\backup`.
    fn configure(&self, guid: &str, enable: bool) -> Result<bool> {
        let info = self.platform.read_device(guid).map_err(|e| {
            log::warn!("apo: cannot read endpoint {guid}: {e:#}");
            user_err(user_text(&e, "Não foi possível ler o aparelho de áudio."))
        })?;
        if enable {
            if !self.platform.apo_installed() {
                return Err(user_err(MSG_NOT_INSTALLED));
            }
            if !can_configure(&info) {
                return Err(user_err(MSG_VIRTUAL));
            }
        }
        let plan = if enable { plan_install(&info.state) } else { plan_uninstall(&info.state) }.map_err(anyhow::Error::new)?;
        if plan.is_noop() {
            if enable {
                self.platform.neutralize_stock_config(); // attached already (DeviceSelector): the stock config applies there too
            }
            return Ok(false);
        }
        self.platform.backup_device(&info.guid).map_err(|e| {
            log::warn!("apo: backup of {} failed: {e:#}", info.guid);
            user_err("Não foi possível guardar a cópia de segurança do aparelho. Nada foi alterado.")
        })?;
        // After the backup, so "Nada foi alterado" is true when the backup fails; the value stays on when a
        // later step fails (APO is installed and needs it).
        if enable {
            self.platform.ensure_protected_audio_dg().map_err(|e| {
                log::warn!("apo: cannot set DisableProtectedAudioDG: {e:#}");
                user_err("Não foi possível liberar o Windows para o Equalizer APO. Nada foi alterado.")
            })?;
        }
        self.mark_dirty();
        log::info!("apo: {} on {} ({} registry operations)", if enable { "attaching" } else { "detaching" }, info.guid, plan.ops.len());
        if let Err(e) = self.platform.apply_plan(&info.guid, &plan) {
            log::error!("apo: applying the plan to {} failed: {e:#}", info.guid);
            let undone = self.rollback(&info);
            return Err(user_err(if undone {
                "O Windows recusou a alteração do aparelho. Ele foi deixado como estava."
            } else {
                "O Windows recusou a alteração do aparelho e não foi possível desfazê-la por completo. A cópia original está na pasta Clarim\\backup, dentro de Arquivos de Programas."
            }));
        }
        if enable {
            self.platform.neutralize_stock_config();
        }
        Ok(true)
    }

    /// Puts `before` back after a failed plan. True when the endpoint is as it was.
    fn rollback(&self, before: &EndpointInfo) -> bool {
        let now = match self.platform.read_device(&before.guid) {
            Ok(now) => now,
            Err(e) => {
                log::error!("apo: rollback of {}: cannot read it: {e:#}", before.guid);
                return false;
            }
        };
        let undo = plan_restore(&before.state, &now.state);
        undo.is_noop() || self.platform.apply_plan(&before.guid, &undo).is_ok()
    }

    /// `SetDevice`: inline, but takes the operation slot, so it never overlaps an install or a restart.
    pub(crate) fn set_device(&self, guid: &str, enabled: bool) -> Result<()> {
        if !is_braced_guid(guid) {
            return Err(user_err(MSG_BAD_REQUEST));
        }
        self.begin(BUSY_DEVICE)?;
        let result = self.configure(guid, enabled);
        // The caller gets the error in the reply; it is not kept as the "last operation" error.
        self.lock().busy = None;
        result.map(|_| ())
    }

    // ---- install ----

    fn op_install(&self, guids: &[String]) -> Outcome {
        let p = &self.platform;
        if !p.apo_installed() {
            self.set_busy(BUSY_DOWNLOAD);
            let installer = p.download_installer(&self.cancel)?;
            let result = (|| -> Result<()> {
                p.verify_installer(&installer)?;
                self.set_busy(BUSY_INSTALL);
                p.run_installer(&installer, &self.cancel)?;
                p.check_installed()
            })();
            p.finish_install(&installer);
            result?;
            p.neutralize_stock_config(); // APO's installer just wrote its stock config.txt
        }
        self.set_busy(BUSY_DEVICES);
        let mut failures = Vec::new();
        for guid in guids {
            if let Err(e) = self.configure(guid, true) {
                failures.push(user_text(&e, "erro desconhecido"));
            }
        }
        match failures.first() {
            None => Ok(None),
            Some(first) if failures.len() == 1 => Err(user_err(format!("Equalizer APO instalado, mas um aparelho não foi configurado: {first}"))),
            Some(first) => Err(user_err(format!(
                "Equalizer APO instalado, mas {} aparelhos não foram configurados. Primeiro erro: {first}",
                failures.len()
            ))),
        }
    }

    /// `InstallApo`: validates, takes the slot, and runs on a worker thread. Returns at once; progress is in
    /// `status().busy`, the result in `status().installed` / `status().error`.
    pub(crate) fn start_install(self: &Arc<Self>, guids: &[String]) -> Result<()> {
        let guids = clean_guids(guids)?;
        self.begin(BUSY_DOWNLOAD)?;
        let me = Arc::clone(self);
        self.run_worker("apo-install", move || {
            let outcome = me.op_install(&guids);
            if let Err(e) = &outcome {
                log::warn!("apo: install failed: {e:#}");
            }
            me.end(outcome);
        })
    }

    // ---- restart ----

    fn op_restart(&self) -> Outcome {
        let not_back = self.platform.restart_audio(&mut |text| self.set_busy(text)).map_err(|e| {
            log::warn!("apo: restarting Windows Audio failed: {e:#}");
            user_err(user_text(&e, "Não foi possível reiniciar o áudio do Windows. Reinicie o computador."))
        })?;
        let mut inner = self.lock();
        inner.dirty_since = None;
        inner.last_restart_end = Some(Instant::now());
        Ok(if not_back.is_empty() {
            None
        } else {
            Some(format!("O áudio voltou, mas estes serviços não: {}. Reinicie o computador se algum programa deixou de funcionar.", not_back.join(", ")))
        })
    }

    /// `RestartAudio`: takes the slot (and refuses a second restart within seconds of the last), runs on a
    /// worker thread, returns at once.
    pub(crate) fn start_restart(self: &Arc<Self>) -> Result<()> {
        if !restart_allowed(self.lock().last_restart_end, Instant::now()) {
            return Err(user_err(MSG_TOO_SOON));
        }
        self.begin(BUSY_RESTART)?;
        let me = Arc::clone(self);
        self.run_worker("apo-restart", move || {
            let outcome = me.op_restart();
            me.end(outcome);
        })
    }

    /// Starts the worker thread; a failure to start gives the operation slot back.
    fn run_worker(&self, name: &str, f: impl FnOnce() + Send + 'static) -> Result<()> {
        std::thread::Builder::new().name(name.into()).spawn(f).map(|_| ()).map_err(|e| {
            log::error!("apo: cannot start the {name} thread: {e}");
            self.lock().busy = None;
            user_err("Não foi possível iniciar a operação. Tente de novo.")
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use anyhow::anyhow;

    use crate::apo_plan::{DeviceRegState, Op, RegValue};

    const REALTEK: &str = "{171dffe0-1f81-4558-a253-45945204afa2}";
    const BT: &str = "{b9cc1e4e-11bd-4f54-8227-5e28f30a485d}";
    const CABLE: &str = "{aee288e6-0000-4000-8000-000000000001}";
    const SFX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},5";
    const MFX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},6";

    fn realtek_state() -> DeviceRegState {
        let mut st = DeviceRegState { fx_key_exists: true, ..DeviceRegState::default() };
        st.fx.insert(SFX.into(), RegValue::Str("{62dc1a93-ae24-464c-a43e-452f824c4250}".into()));
        st.fx.insert(MFX.into(), RegValue::Str("{637c490d-eee3-4c0a-973f-371958802da2}".into()));
        st
    }

    fn endpoint(guid: &str, name: &str, state: DeviceRegState) -> EndpointInfo {
        EndpointInfo { guid: guid.into(), name: name.into(), device_state: 1, state, fx_last_write: Some(100), sysfx_in_properties: false }
    }

    /// A pretend machine: devices whose registry state follows the plans, a clock, scripted failures, and a
    /// log of what was asked.
    struct Fake {
        apo_installed: AtomicBool,
        devices: Mutex<Vec<EndpointInfo>>,
        log: Mutex<Vec<String>>,
        now: AtomicUsize,
        audiodg: Mutex<AudioDg>,
        fail_download: AtomicBool,
        fail_verify: AtomicBool,
        fail_install: AtomicBool,
        fail_apply_after: Mutex<Option<usize>>,
        fail_backup: AtomicBool,
        fail_restart: AtomicBool,
        restart_leaves: Mutex<Vec<String>>,
        neutralized: AtomicUsize,
    }

    impl Fake {
        fn new(devices: Vec<EndpointInfo>) -> Fake {
            Fake {
                apo_installed: AtomicBool::new(true),
                devices: Mutex::new(devices),
                log: Mutex::new(Vec::new()),
                now: AtomicUsize::new(1000),
                audiodg: Mutex::new(AudioDg { starts: Some(vec![]), apo_loaded: None }),
                fail_download: AtomicBool::new(false),
                fail_verify: AtomicBool::new(false),
                fail_install: AtomicBool::new(false),
                fail_apply_after: Mutex::new(None),
                fail_backup: AtomicBool::new(false),
                fail_restart: AtomicBool::new(false),
                restart_leaves: Mutex::new(Vec::new()),
                neutralized: AtomicUsize::new(0),
            }
        }

        fn note(&self, s: &str) {
            self.log.lock().unwrap().push(s.to_string());
        }

        fn calls(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn state_of(&self, guid: &str) -> DeviceRegState {
            self.devices.lock().unwrap().iter().find(|d| d.guid == guid).unwrap().state.clone()
        }
    }

    impl Platform for Arc<Fake> {
        fn apo_installed(&self) -> bool {
            self.apo_installed.load(Ordering::Relaxed)
        }
        fn list_devices(&self) -> Result<Vec<EndpointInfo>> {
            Ok(self.devices.lock().unwrap().clone())
        }
        fn read_device(&self, guid: &str) -> Result<EndpointInfo> {
            if !is_braced_guid(guid) {
                return Err(user_err("Identificador de dispositivo inválido."));
            }
            self.devices
                .lock()
                .unwrap()
                .iter()
                .find(|d| d.guid.eq_ignore_ascii_case(guid))
                .cloned()
                .ok_or_else(|| user_err("Aparelho de áudio não encontrado."))
        }
        fn backup_device(&self, guid: &str) -> Result<()> {
            if self.fail_backup.load(Ordering::Relaxed) {
                return Err(anyhow!("disk full"));
            }
            self.note(&format!("backup {guid}"));
            Ok(())
        }
        fn apply_plan(&self, guid: &str, plan: &Plan) -> Result<()> {
            self.note(&format!("apply {guid} ({} ops)", plan.ops.len()));
            let mut devices = self.devices.lock().unwrap();
            let device = devices.iter_mut().find(|d| d.guid == guid).unwrap();
            let limit = self.fail_apply_after.lock().unwrap().take();
            match limit {
                Some(n) => {
                    Plan { ops: plan.ops[..n].to_vec() }.simulate(&mut device.state);
                    Err(anyhow!("access denied"))
                }
                None => {
                    plan.simulate(&mut device.state);
                    Ok(())
                }
            }
        }
        fn ensure_protected_audio_dg(&self) -> Result<bool> {
            self.note("protected-dg");
            Ok(false)
        }
        fn download_installer(&self, _cancel: &AtomicBool) -> Result<PathBuf> {
            self.note("download");
            if self.fail_download.load(Ordering::Relaxed) {
                return Err(user_err("Sem internet."));
            }
            Ok(PathBuf::from("installer.exe"))
        }
        fn verify_installer(&self, _path: &Path) -> Result<()> {
            self.note("verify");
            if self.fail_verify.load(Ordering::Relaxed) {
                return Err(user_err("hash diferente"));
            }
            Ok(())
        }
        fn run_installer(&self, _path: &Path, _cancel: &AtomicBool) -> Result<()> {
            self.note("run-installer");
            if self.fail_install.load(Ordering::Relaxed) {
                return Err(user_err("demorou demais"));
            }
            self.apo_installed.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn check_installed(&self) -> Result<()> {
            self.note("check");
            Ok(())
        }
        fn finish_install(&self, _installer: &Path) {
            self.note("finish-install");
        }
        fn restart_audio(&self, progress: &mut dyn FnMut(&str)) -> Result<Vec<String>> {
            self.note("restart");
            progress("passo");
            if self.fail_restart.load(Ordering::Relaxed) {
                return Err(user_err("Reinicie o computador."));
            }
            Ok(self.restart_leaves.lock().unwrap().clone())
        }
        fn audiodg(&self) -> AudioDg {
            self.audiodg.lock().unwrap().clone()
        }
        fn neutralize_stock_config(&self) {
            self.neutralized.fetch_add(1, Ordering::Relaxed);
        }
        fn now(&self) -> u64 {
            self.now.load(Ordering::Relaxed) as u64
        }
    }

    fn service(devices: Vec<EndpointInfo>) -> (Arc<ApoService<Arc<Fake>>>, Arc<Fake>) {
        let fake = Arc::new(Fake::new(devices));
        (ApoService::new(Arc::clone(&fake)), fake)
    }

    fn standard() -> (Arc<ApoService<Arc<Fake>>>, Arc<Fake>) {
        service(vec![
            endpoint(REALTEK, "Alto-falantes (Realtek(R) Audio)", realtek_state()),
            endpoint(BT, "Fones (WZ-BT5.0)", realtek_state()),
            endpoint(CABLE, "CABLE Input (VB-Audio Virtual Cable)", DeviceRegState::default()),
        ])
    }

    /// Runs an install the way the worker thread would, without a thread.
    fn install_inline(svc: &ApoService<Arc<Fake>>, guids: &[&str]) -> Outcome {
        let guids: Vec<String> = guids.iter().map(|g| g.to_string()).collect();
        svc.begin(BUSY_DOWNLOAD)?;
        let outcome = svc.op_install(&guids);
        let copy = outcome.as_ref().map(Clone::clone).map_err(|e| anyhow!("{e:#}"));
        svc.end(outcome);
        copy
    }

    fn wait_idle(svc: &ApoService<Arc<Fake>>) -> ApoServiceStatus {
        for _ in 0..200 {
            let st = svc.status();
            if st.busy.is_none() {
                return st;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("the operation never finished");
    }

    #[test]
    fn status_lists_devices_with_configured_and_can_configure() {
        let (svc, fake) = standard();
        svc.set_device(REALTEK, true).unwrap();
        let st = svc.status();
        assert!(st.installed && st.busy.is_none() && st.error.is_none());
        let by = |g: &str| st.devices.iter().find(|d| d.guid == g).unwrap().clone();
        assert!(by(REALTEK).configured && by(REALTEK).can_configure);
        assert!(!by(BT).configured && by(BT).can_configure);
        assert!(!by(CABLE).can_configure, "a virtual cable is never offered");
        // a device that is gone is not configurable
        fake.devices.lock().unwrap()[1].device_state = 4;
        assert!(!svc.status().devices[1].can_configure);
    }

    #[test]
    fn set_device_attaches_backs_up_first_and_detaches_exactly() {
        let (svc, fake) = standard();
        let before = fake.state_of(REALTEK);
        svc.set_device(REALTEK, true).unwrap();
        let ops = plan_install(&before).unwrap().ops.len();
        assert_eq!(fake.calls(), [format!("backup {REALTEK}"), "protected-dg".to_string(), format!("apply {REALTEK} ({ops} ops)")]);
        assert_ne!(fake.state_of(REALTEK), before);
        svc.set_device(REALTEK, true).unwrap(); // already attached: nothing to do, no second backup
        assert_eq!(fake.calls().len(), 3);
        svc.set_device(REALTEK, false).unwrap();
        assert_eq!(fake.state_of(REALTEK), before, "off again: the registry is as it was");
        assert!(!svc.status().devices[0].configured);
        svc.set_device(REALTEK, false).unwrap(); // detaching what is not attached is fine
        assert!(svc.status().busy.is_none(), "the slot is free after every call");
    }

    #[test]
    fn set_device_refuses_what_it_should() {
        let (svc, fake) = standard();
        for bad in ["", "{x}", "all", &format!("{REALTEK}\nInclude: x"), "{0.0.0.00000000}.{171dffe0-1f81-4558-a253-45945204afa2}"] {
            assert_eq!(svc.set_device(bad, true).unwrap_err().to_string(), "Pedido inválido.", "{bad:?}");
        }
        let unknown = "{00000000-0000-0000-0000-00000000dead}";
        assert_eq!(svc.set_device(unknown, true).unwrap_err().to_string(), "Aparelho de áudio não encontrado.");
        assert!(svc.set_device(CABLE, true).unwrap_err().to_string().contains("virtual"));
        fake.apo_installed.store(false, Ordering::Relaxed);
        assert_eq!(svc.set_device(REALTEK, true).unwrap_err().to_string(), MSG_NOT_INSTALLED);
        assert!(fake.calls().is_empty(), "nothing touched the registry: {:?}", fake.calls());
        // an orphan (attached, no record) cannot be undone: refused with the explanation, nothing changed
        fake.apo_installed.store(true, Ordering::Relaxed);
        fake.devices.lock().unwrap()[0].state.fx.insert(SFX.into(), RegValue::Str(crate::apo_plan::APO_PRE.into()));
        assert!(svc.set_device(REALTEK, false).unwrap_err().to_string().contains("desinstalador"));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_failed_backup_changes_nothing_and_a_failed_apply_is_rolled_back() {
        let (svc, fake) = standard();
        let before = fake.state_of(REALTEK);
        fake.fail_backup.store(true, Ordering::Relaxed);
        assert!(svc.set_device(REALTEK, true).unwrap_err().to_string().contains("cópia de segurança"));
        assert_eq!(fake.state_of(REALTEK), before);
        assert!(!fake.calls().iter().any(|c| c.starts_with("apply") || c == "protected-dg"), "the Windows-wide switch is not touched before the backup exists");
        fake.fail_backup.store(false, Ordering::Relaxed);

        let total = plan_install(&before).unwrap().ops.len();
        for cut in [1, 5, total - 1] {
            *fake.fail_apply_after.lock().unwrap() = Some(cut);
            let err = svc.set_device(REALTEK, true).unwrap_err().to_string();
            assert!(err.contains("deixado como estava"), "{err}");
            assert_eq!(fake.state_of(REALTEK), before, "cut after {cut} ops: rolled back");
        }
        // and the device can still be set up normally afterwards
        svc.set_device(REALTEK, true).unwrap();
        assert_eq!(classify(&fake.state_of(REALTEK)), Attach::Installed);
    }

    #[test]
    fn install_downloads_checks_runs_then_attaches_the_devices_in_order() {
        let (svc, fake) = standard();
        fake.apo_installed.store(false, Ordering::Relaxed);
        install_inline(&svc, &[REALTEK, BT]).unwrap();
        let calls = fake.calls();
        let pos = |name: &str| calls.iter().position(|c| c.starts_with(name)).unwrap_or_else(|| panic!("{name} missing in {calls:?}"));
        assert!(pos("download") < pos("verify") && pos("verify") < pos("run-installer") && pos("run-installer") < pos("check"));
        assert!(pos("check") < pos("finish-install") && pos("finish-install") < pos(&format!("backup {REALTEK}")));
        assert!(pos(&format!("apply {REALTEK}")) < pos(&format!("backup {BT}")));
        let st = svc.status();
        assert!(st.installed && st.busy.is_none() && st.error.is_none());
        assert!(st.devices.iter().filter(|d| d.configured).count() == 2);
    }

    #[test]
    fn apo_stock_config_is_neutralized_when_a_device_is_attached_or_apo_is_installed() {
        let neutralized = |fake: &Fake| fake.neutralized.load(Ordering::Relaxed);
        let (svc, fake) = standard();
        svc.set_device(REALTEK, true).unwrap();
        assert_eq!(neutralized(&fake), 1, "after a real attach");
        svc.set_device(REALTEK, true).unwrap();
        assert_eq!(neutralized(&fake), 2, "also when the device was attached already (DeviceSelector)");
        svc.set_device(REALTEK, false).unwrap();
        assert_eq!(neutralized(&fake), 2, "detaching leaves the config alone");

        // refused or rolled back attaches never reach it
        assert!(svc.set_device(CABLE, true).is_err());
        fake.fail_backup.store(true, Ordering::Relaxed);
        assert!(svc.set_device(BT, true).is_err());
        fake.fail_backup.store(false, Ordering::Relaxed);
        *fake.fail_apply_after.lock().unwrap() = Some(1);
        assert!(svc.set_device(BT, true).is_err());
        assert_eq!(neutralized(&fake), 2);

        // a fresh install: once, right after the installer (before any device), even with no device to attach
        let (svc, fake) = standard();
        fake.apo_installed.store(false, Ordering::Relaxed);
        install_inline(&svc, &[]).unwrap();
        assert_eq!(neutralized(&fake), 1);
        // a failed installer leaves it alone
        let (svc, fake) = standard();
        fake.apo_installed.store(false, Ordering::Relaxed);
        fake.fail_install.store(true, Ordering::Relaxed);
        assert!(install_inline(&svc, &[REALTEK]).is_err());
        assert_eq!(neutralized(&fake), 0);
    }

    #[test]
    fn install_stops_at_the_first_failed_step_and_still_cleans_up() {
        let (svc, fake) = standard();
        fake.apo_installed.store(false, Ordering::Relaxed);

        fake.fail_download.store(true, Ordering::Relaxed);
        assert!(install_inline(&svc, &[REALTEK]).is_err());
        assert_eq!(svc.status().error.as_deref(), Some("Sem internet."), "the PT-BR text reaches the status");
        assert_eq!(fake.calls(), ["download"], "nothing else ran, nothing to clean up");
        fake.fail_download.store(false, Ordering::Relaxed);
        fake.log.lock().unwrap().clear();

        fake.fail_verify.store(true, Ordering::Relaxed);
        assert!(install_inline(&svc, &[REALTEK]).is_err());
        assert_eq!(fake.calls(), ["download", "verify", "finish-install"], "a file that failed the hash check is never run");
        assert_eq!(svc.status().error.as_deref(), Some("hash diferente"));
        fake.fail_verify.store(false, Ordering::Relaxed);
        fake.log.lock().unwrap().clear();

        fake.fail_install.store(true, Ordering::Relaxed);
        assert!(install_inline(&svc, &[REALTEK]).is_err());
        assert_eq!(fake.calls(), ["download", "verify", "run-installer", "finish-install"]);
        assert!(!svc.status().installed && svc.status().busy.is_none());
        assert_eq!(fake.state_of(REALTEK), realtek_state(), "no device was touched");
    }

    #[test]
    fn install_when_apo_is_already_there_only_configures_and_reports_device_failures() {
        let (svc, fake) = standard();
        let err = install_inline(&svc, &[REALTEK, CABLE, BT]).unwrap_err().to_string();
        assert!(err.contains("Equalizer APO instalado, mas um aparelho não foi configurado") && err.contains("virtual"), "{err}");
        assert!(!fake.calls().iter().any(|c| c == "download" || c == "run-installer"), "{:?}", fake.calls());
        let st = svc.status();
        assert!(st.devices.iter().find(|d| d.guid == REALTEK).unwrap().configured, "the others were still done");
        assert!(st.devices.iter().find(|d| d.guid == BT).unwrap().configured);
        assert!(st.error.as_deref().is_some_and(|e| e.contains("virtual")));
        // starting something else clears the old error
        svc.set_device(BT, false).unwrap();
        assert!(svc.begin("x").is_ok() && svc.status().error.is_none());
    }

    #[test]
    fn install_requests_are_validated_before_anything_starts() {
        let (svc, fake) = standard();
        let g = |s: &str| s.to_string();
        for bad in [vec![g("{x}")], vec![g(REALTEK), g("not a guid")], (0..17).map(|_| g(REALTEK)).collect::<Vec<_>>()] {
            assert_eq!(svc.start_install(&bad).unwrap_err().to_string(), "Pedido inválido.");
        }
        assert!(svc.status().busy.is_none() && fake.calls().is_empty());
        assert_eq!(clean_guids(&[g(REALTEK), g(&REALTEK.to_uppercase()), g(BT)]).unwrap(), [REALTEK, BT], "duplicates dropped, order kept");
        assert!(clean_guids(&[]).unwrap().is_empty(), "no devices is valid: install the program only");
    }

    #[test]
    fn only_one_operation_runs_at_a_time_and_the_busy_text_is_reported() {
        let (svc, _fake) = standard();
        svc.begin(BUSY_INSTALL).unwrap();
        let st = svc.status();
        assert_eq!(st.busy.as_deref(), Some(BUSY_INSTALL));
        for refused in [
            svc.set_device(REALTEK, true).unwrap_err(),
            svc.start_install(&[]).unwrap_err(),
            svc.start_restart().unwrap_err(),
        ] {
            let text = refused.to_string();
            assert!(text.contains("Já há uma operação em andamento") && text.contains(BUSY_INSTALL), "{text}");
        }
        svc.end(Ok(None));
        assert!(svc.status().busy.is_none());
        svc.set_device(REALTEK, true).expect("free again");
    }

    #[test]
    fn stopping_waits_for_the_running_operation_and_lets_nothing_new_start() {
        let (svc, fake) = standard();
        // idle: nothing to wait for
        assert!(svc.wait_until_idle(Duration::from_millis(50)));
        // a running operation (an Audiosrv restart half way, say) is waited for, and only as long as asked
        svc.begin(BUSY_RESTART).unwrap();
        svc.shutdown();
        assert!(!svc.wait_until_idle(Duration::from_millis(150)), "still running at the deadline");
        let worker = {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                svc.end(Ok(None));
            })
        };
        assert!(svc.wait_until_idle(Duration::from_secs(10)), "returns as soon as the operation ends");
        worker.join().unwrap();
        // after shutdown every way in is refused, so the process cannot exit in the middle of a new one
        for refused in [svc.set_device(REALTEK, true).unwrap_err(), svc.start_install(&[]).unwrap_err(), svc.start_restart().unwrap_err()] {
            assert_eq!(refused.to_string(), MSG_STOPPING);
        }
        assert!(fake.calls().is_empty() && svc.status().busy.is_none());
    }

    #[test]
    fn install_and_restart_run_on_worker_threads_and_finish() {
        let (svc, fake) = standard();
        fake.apo_installed.store(false, Ordering::Relaxed);
        svc.start_install(&[REALTEK.to_string()]).unwrap();
        let st = wait_idle(&svc);
        assert!(st.installed && st.error.is_none() && st.devices[0].configured);

        svc.start_restart().unwrap();
        let st = wait_idle(&svc);
        assert!(st.error.is_none());
        assert!(fake.calls().contains(&"restart".to_string()));
        // a second restart right away is refused (any local user may ask)
        assert_eq!(svc.start_restart().unwrap_err().to_string(), MSG_TOO_SOON);
    }

    #[test]
    fn restart_failures_and_stragglers_are_reported_in_portuguese() {
        let (svc, fake) = standard();
        fake.fail_restart.store(true, Ordering::Relaxed);
        svc.begin(BUSY_RESTART).unwrap();
        let outcome = svc.op_restart();
        assert!(outcome.is_err());
        svc.end(outcome);
        assert_eq!(svc.status().error.as_deref(), Some("Reinicie o computador."));
        assert!(svc.lock().last_restart_end.is_none(), "a failed restart does not start the cool-down");

        fake.fail_restart.store(false, Ordering::Relaxed);
        *fake.restart_leaves.lock().unwrap() = vec!["LightingService".into()];
        svc.begin(BUSY_RESTART).unwrap();
        let outcome = svc.op_restart();
        svc.end(outcome);
        let error = svc.status().error.unwrap();
        assert!(error.contains("LightingService") && error.contains("O áudio voltou"), "{error}");
    }

    #[test]
    fn restart_is_needed_only_while_a_running_audio_graph_predates_the_change() {
        let starts = |v: &[u64]| Some(v.to_vec());
        // the graph started before the change: needed; after it: not
        assert!(restart_needed(Some(500), starts(&[100]).as_deref(), false));
        assert!(!restart_needed(Some(500), starts(&[900]).as_deref(), true));
        assert!(restart_needed(Some(500), starts(&[100, 900]).as_deref(), false), "any old one is enough");
        assert!(restart_needed(Some(500), starts(&[500]).as_deref(), false), "same tick counts as before");
        // no graph running: the next one will be fresh
        assert!(!restart_needed(Some(500), starts(&[]).as_deref(), true));
        // nothing changed
        assert!(!restart_needed(None, starts(&[100]).as_deref(), false));
        // cannot read the graphs: our own flag decides
        assert!(restart_needed(None, None, true) && restart_needed(Some(5), None, true));
        assert!(!restart_needed(None, None, false) && !restart_needed(Some(5), None, false));
    }

    #[test]
    fn status_needs_restart_follows_our_changes_and_the_graph_start_times() {
        let (svc, fake) = standard();
        *fake.audiodg.lock().unwrap() = AudioDg { starts: Some(vec![500]), apo_loaded: Some(false) };
        fake.now.store(1000, Ordering::Relaxed);
        assert!(!svc.status().needs_restart, "nothing configured, nothing changed");
        svc.set_device(REALTEK, true).unwrap(); // changed at 1000, the graph started at 500
        let st = svc.status();
        assert!(st.needs_restart && st.apo_loaded == Some(false));
        // a graph that started after the change picked it up: no restart, and the flag is dropped
        *fake.audiodg.lock().unwrap() = AudioDg { starts: Some(vec![2000]), apo_loaded: Some(true) };
        assert!(!svc.status().needs_restart);
        assert!(svc.lock().dirty_since.is_none());
        // another change, then a real restart: the flag is gone and the new graph is newer than every write
        *fake.audiodg.lock().unwrap() = AudioDg { starts: Some(vec![500]), apo_loaded: None };
        fake.now.store(3000, Ordering::Relaxed);
        svc.set_device(BT, true).unwrap();
        assert!(svc.status().needs_restart);
        svc.begin(BUSY_RESTART).unwrap();
        let outcome = svc.op_restart();
        svc.end(outcome);
        *fake.audiodg.lock().unwrap() = AudioDg { starts: Some(vec![4000]), apo_loaded: None };
        assert!(!svc.status().needs_restart);
    }

    #[test]
    fn a_device_configured_by_deviceselector_needs_a_restart_when_its_registry_write_is_newer_than_the_graph() {
        let mut st = realtek_state();
        st.fx.insert(SFX.into(), RegValue::Str(crate::apo_plan::APO_PRE.into()));
        let mut child = crate::apo_plan::Values::new();
        child.insert("version".into(), RegValue::Str("2".into()));
        st.child = Some(child);
        let mut info = endpoint(REALTEK, "Alto-falantes (Realtek(R) Audio)", st);
        info.fx_last_write = Some(5000);
        let (svc, fake) = service(vec![info]);
        *fake.audiodg.lock().unwrap() = AudioDg { starts: Some(vec![4000]), apo_loaded: Some(false) };
        let status = svc.status();
        assert!(status.devices[0].configured && status.needs_restart, "configured at 5000, graph from 4000");
        *fake.audiodg.lock().unwrap() = AudioDg { starts: Some(vec![6000]), apo_loaded: Some(true) };
        assert!(!svc.status().needs_restart);
    }

    #[test]
    fn plans_only_ever_name_the_endpoint_that_was_asked_for() {
        // every op the service ever hands to the registry layer carries registry value names, never a path
        let plan = plan_install(&realtek_state()).unwrap();
        for op in &plan.ops {
            if let Op::SetFx(n, _) | Op::DeleteFx(n) | Op::SetChild(n, _) | Op::DeleteChild(n) = op {
                assert!(!n.contains('\\') && !n.contains('/'), "{n}");
            }
        }
    }

    #[test]
    fn filetime_conversion_matches_the_epoch_offsets() {
        assert_eq!(filetime_of(UNIX_EPOCH), 116_444_736_000_000_000);
        assert_eq!(filetime_of(UNIX_EPOCH + Duration::from_secs(1)), 116_444_736_010_000_000);
    }
}
