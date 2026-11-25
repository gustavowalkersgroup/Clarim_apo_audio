//! Mic engine facade used by the app: resolves devices, starts / stops / restarts the pipeline when
//! devices or `enabled` change, pushes DSP-only changes live (no glitch), runs voice calibration.

use crate::calib::CalibShared;
use crate::chain::DspParams;
use crate::devices::{self, Flow};
use crate::pipeline::{self, lock, Counters, EngineCmd, EngineConfig, EngineHandle};
use crate::refs::{Loopbacks, RefSlots};
use clarim_core::{DeviceInfo, MicSettings, MicStatus};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasapi::{Direction, Role};

pub const NO_VIRTUAL_MIC: &str = "Nenhum microfone virtual encontrado — instale o VB-Cable (https://vb-audio.com/Cable/)";
const CALIBRATION_FAILED: &str = "Calibração falhou: fale mais alto/perto do microfone";
const CALIBRATION_INACTIVE: &str = "Calibração indisponível: o microfone não está ativo";
/// The dashboard recognises the soft notes below by their first words ("Calibração", "Microfone
/// silenciado", "Ouvir meu microfone") and keeps them out of the red error state: keep the prefixes.
const MIC_SILENT: &str = "Microfone silenciado ou bloqueado pelo Windows (Configurações > Privacidade > Microfone)";
const MONITOR_DOWN: &str = "Ouvir meu microfone indisponível: a saída escolhida não foi encontrada ou parou (fones desconectados?). Desligue e ligue a opção para tentar de novo.";
/// How long a calibration message stays in `status().error`.
const NOTE_TTL: Duration = Duration::from_secs(10);
/// How often `status()` writes the stream health line to the log.
const STATS_EVERY: Duration = Duration::from_secs(10);
/// How long stopping the loopback streams may wait for their threads: at quit, and when the mic is switched
/// off (that one runs under the backend's mic lock, so it waits less). They normally exit within ~30 ms.
const LOOPBACK_QUIT_WAIT: Duration = Duration::from_millis(300);
const LOOPBACK_OFF_WAIT: Duration = Duration::from_millis(100);

struct Resolved {
    input: DeviceInfo,
    output: DeviceInfo,
    /// Where "Ouvir meu microfone" plays; `None` while monitoring is off (or its device is gone).
    monitor: Option<DeviceInfo>,
}

pub struct MicEngine {
    deny: Vec<String>,
    handle: Option<EngineHandle>,
    /// Settings of the last `apply` (whose changes were fully delivered).
    applied: Option<MicSettings>,
    resolved: Option<Resolved>,
    /// Why no stream could be started (no virtual mic, device missing, feedback guard, ...).
    resolve_error: Option<String>,
    calib: Arc<CalibShared>,
    /// Short-lived calibration message.
    note: Mutex<Option<(Instant, &'static str)>>,
    /// When `status()` last logged the stream counters, and what they were then.
    stats_logged: Mutex<(Instant, Counters)>,
    /// Speaker references: the slots every pipeline of this engine reads, and the loopback streams that
    /// fill them. The streams belong to the engine, not to a pipeline: a mic restart leaves them running.
    refs: Arc<RefSlots>,
    loopbacks: Loopbacks,
}

impl MicEngine {
    /// Idle: no streams are opened until `apply` enables the mic.
    pub fn new(deny: Vec<String>) -> Self {
        let refs = Arc::new(RefSlots::new());
        Self {
            deny,
            handle: None,
            applied: None,
            resolved: None,
            resolve_error: None,
            calib: Arc::new(CalibShared::new()),
            note: Mutex::new(None),
            stats_logged: Mutex::new((Instant::now(), Counters::default())),
            loopbacks: Loopbacks::new(refs.clone()),
            refs,
        }
    }

    /// Start/stop/restart streams if devices or `enabled` changed (or the streams died: calling again
    /// with the same settings retries); otherwise push the DSP parameters live.
    pub fn apply(&mut self, s: &MicSettings) {
        if !s.enabled {
            self.shutdown();
            self.loopbacks.stop_all(LOOPBACK_OFF_WAIT);
            self.applied = Some(s.clone());
            return;
        }
        let running = self.handle.as_ref().is_some_and(EngineHandle::is_running);
        // The monitor thread exists only while monitoring is on, so toggling it (or changing its device
        // while on) restarts the streams; its device is irrelevant while off.
        let monitor = |p: &MicSettings| p.monitor_on.then(|| p.monitor_id.clone());
        let same_devices = self.applied.as_ref().is_some_and(|p| {
            p.enabled && p.input_id == s.input_id && p.output_id == s.output_id && monitor(p) == monitor(s)
        });
        if running && same_devices {
            if self.applied.as_ref() == Some(s) {
                return;
            }
            let sent = self.handle.as_ref().is_some_and(|h| h.send(EngineCmd::SetParams(DspParams::from_settings(s))));
            if sent {
                self.applied = Some(s.clone());
            } else {
                log::warn!("mic engine: live update not delivered, will retry on the next apply");
            }
            return;
        }

        self.shutdown();
        self.applied = Some(s.clone());
        match resolve(s, &self.deny) {
            Ok(r) => {
                log::info!(
                    "mic engine: starting, input '{}', virtual mic '{}', monitor '{}'",
                    r.input.name,
                    r.output.name,
                    r.monitor.as_ref().map_or("off", |m| m.name.as_str())
                );
                let cfg = EngineConfig {
                    input_id: Some(r.input.id.clone()),
                    output_id: r.output.id.clone(),
                    monitor_id: r.monitor.as_ref().map(|m| m.id.clone()),
                    params: DspParams::from_settings(s),
                    calib: self.calib.clone(),
                    refs: self.refs.clone(),
                };
                self.resolved = Some(r);
                match pipeline::start(cfg) {
                    Ok(h) => self.handle = Some(h),
                    Err(e) => self.resolve_error = Some(format!("Falha ao iniciar o áudio: {e:#}")),
                }
            }
            Err(msg) => {
                log::warn!("mic engine: not started: {msg}");
                self.resolve_error = Some(msg);
            }
        }
    }

    /// Every `MicStatus` field. `denoise_active` is `None` until the DSP thread has built its chain
    /// (about 0.5 s in strong mode); `focus_open` is false while Voice Focus is off; levels and
    /// `dsp_us` / `latency_ms` are 0 while not running. `latency_ms` is an ESTIMATE of our own
    /// mic-to-cable delay (see `Meters::latency_ms`): without the audio engine, the cable driver and the
    /// buffers of the app that reads the cable. Soft notes (`error`, shown only when nothing is wrong)
    /// say so when Windows delivers only silence and when the monitor cannot play.
    pub fn status(&self) -> MicStatus {
        let mut st = MicStatus {
            calibrating: self.calib.calibrating(),
            calibration_progress: self.calib.progress(),
            input_name: self.resolved.as_ref().map(|r| r.input.name.clone()),
            output_name: self.resolved.as_ref().map(|r| r.output.name.clone()),
            references: self.loopbacks.running(),
            ..MicStatus::default()
        };
        if self.calib.take_failed() {
            *lock(&self.note) = Some((Instant::now(), CALIBRATION_FAILED));
        }
        let mut error = self.resolve_error.clone();
        if let Some(h) = &self.handle {
            st.running = h.is_running();
            error = h.error().or(error);
            if st.running {
                let m = &h.meters;
                st.denoise_active = m.denoise_active();
                st.in_peak = m.in_peak();
                st.out_peak = m.out_peak();
                st.vad = m.vad();
                st.focus_open = m.focus_open();
                st.dsp_us = m.dsp_us();
                st.latency_ms = m.latency_ms();
                st.agc_gain_db = m.agc_gain_db();
                st.input_voice_db = m.input_voice_db();
                st.voice_db = m.voice_db();
                st.echo_score = m.echo_score();
                st.echo_db = m.echo_db();
                st.echo_lag_ms = m.echo_lag_ms();
                st.noise_db = m.noise_db();
                st.clip_events = m.clip_frames();
                if error.is_none() && m.mic_silent() {
                    error = Some(MIC_SILENT.to_string());
                }
                let wanted = self.applied.as_ref().is_some_and(|a| a.monitor_on);
                let resolved = self.resolved.as_ref().is_some_and(|r| r.monitor.is_some());
                if error.is_none() && monitor_down(wanted, resolved, m.monitor_failed()) {
                    error = Some(MONITOR_DOWN.to_string());
                }
                self.log_stats(m.counters(), st.latency_ms, st.dsp_us);
            }
        }
        if error.is_none() {
            if let Some((at, msg)) = *lock(&self.note) {
                if at.elapsed() < NOTE_TTL {
                    error = Some(msg.to_string());
                }
            }
        }
        st.error = error;
        st
    }

    /// Stream health counters of the running engine (`None` while stopped).
    pub fn counters(&self) -> Option<Counters> {
        self.handle.as_ref().filter(|h| h.is_running()).map(|h| h.meters.counters())
    }

    /// Every `STATS_EVERY`: one debug line with the numbers, and a warning when a counter grew since the
    /// last line (underruns and overruns are audible glitches; slips are the normal clock-drift fix).
    fn log_stats(&self, now: Counters, latency_ms: f32, dsp_us: u32) {
        let mut last = lock(&self.stats_logged);
        if last.0.elapsed() < STATS_EVERY {
            return;
        }
        let (underruns, overruns) = (now.underruns.saturating_sub(last.1.underruns), now.overruns.saturating_sub(last.1.overruns));
        if underruns + overruns > 0 {
            log::warn!("mic stream glitches in the last {STATS_EVERY:?}: {underruns} underruns, {overruns} overruns");
        }
        log::debug!(
            "mic stats: latency ~{latency_ms:.1} ms, dsp {dsp_us} us, underruns {}, overruns {}, slips {}",
            now.underruns,
            now.overruns,
            now.slips
        );
        *last = (Instant::now(), now);
    }

    /// Collect speech frames (VAD > 0.6) for `seconds`; the result arrives via `take_calibration`.
    pub fn start_calibration(&self, seconds: f32) {
        if self.handle.as_ref().is_some_and(EngineHandle::is_running) {
            *lock(&self.note) = None;
            self.calib.request(seconds);
        } else {
            *lock(&self.note) = Some((Instant::now(), CALIBRATION_INACTIVE));
        }
    }

    /// `Some(dBFS)` once, after a calibration completes (70th percentile of speech-frame RMS dB).
    pub fn take_calibration(&self) -> Option<f32> {
        self.calib.take_result()
    }

    pub fn stop(&mut self) {
        self.shutdown();
        self.loopbacks.stop_all(LOOPBACK_QUIT_WAIT);
        self.applied = None;
    }

    /// Make exactly these render endpoints (at most `MAX_REFS`) the speaker references of the echo
    /// detector. Idempotent and cheap: call it every housekeeping tick. Never joins a thread, so it is safe
    /// under the caller's lock. Ignored while the mic is disabled (the streams were stopped with it).
    pub fn set_references(&mut self, ids: &[String]) {
        if self.applied.as_ref().is_some_and(|a| a.enabled) {
            self.loopbacks.set(ids);
        }
    }

    /// The mic's level just changed by `db` outside the chain (the Windows mic volume): the voice AGC moves
    /// its learned level by the same amount. False when no pipeline runs or its command queue is full.
    pub fn shift_mic_volume(&self, db: f32) -> bool {
        db.is_finite() && self.handle.as_ref().is_some_and(|h| h.send(EngineCmd::MicVolumeShift(db)))
    }

    /// Endpoint id of the real mic in use (`None` unless the pipeline is running).
    pub fn input_device_id(&self) -> Option<String> {
        self.running_devices().map(|r| r.input.id.clone())
    }

    /// Endpoint id of the virtual mic's render side in use (`None` unless the pipeline is running).
    pub fn output_device_id(&self) -> Option<String> {
        self.running_devices().map(|r| r.output.id.clone())
    }

    /// Endpoint id "Ouvir meu microfone" plays on: the RESOLVED device (the Windows default output when no
    /// device was chosen). `None` while monitoring is off or the pipeline is not running.
    pub fn monitor_device_id(&self) -> Option<String> {
        self.running_devices().and_then(|r| r.monitor.as_ref()).map(|m| m.id.clone())
    }

    fn running_devices(&self) -> Option<&Resolved> {
        self.handle.as_ref().filter(|h| h.is_running()).and(self.resolved.as_ref())
    }

    fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            h.stop();
        }
        self.calib.cancel();
        self.resolved = None;
        self.resolve_error = None;
        *lock(&self.stats_logged) = (Instant::now(), Counters::default());
    }
}

/// "Ouvir meu microfone" is on but nothing can play: no output was found for it (`resolved` false) or its
/// stream stopped. The mic and the cable keep running, so this is a note, not an error, and it never
/// restarts the engine (a restart would cut the mic in a call just because the headphones are away).
fn monitor_down(wanted: bool, resolved: bool, failed: bool) -> bool {
    wanted && (!resolved || failed)
}

/// Enumerate the endpoints and pick for these settings (see `pick`). The error is a PT-BR sentence for
/// the dashboard.
fn resolve(s: &MicSettings, deny: &[String]) -> Result<Resolved, String> {
    // This thread may be a plain worker (e.g. an HTTP request thread): make sure it has COM.
    // Fails only when the thread already runs STA COM, which works for enumeration too.
    let _ = crate::init_com();
    let list = |flow| {
        devices::enumerate(flow, &[]).map_err(|e| {
            let detail = format!("{e:#}");
            if crate::recovery::is_audio_service_loss(&detail) {
                log::debug!("mic engine: Windows Audio is not answering yet: {detail}");
                return crate::recovery::RECONNECTING_TEXT.to_string();
            }
            format!("Falha ao listar os dispositivos de áudio: {detail}")
        })
    };
    pick(s, deny, &list(Flow::Capture)?, &list(Flow::Render)?)
}

/// Pick the real mic and the virtual-mic render endpoint from the full device lists. The user's
/// explicit ids are looked up in the full lists (the deny-list only hides devices from pickers); the
/// automatic choices honour it.
fn pick(s: &MicSettings, deny: &[String], all_in: &[DeviceInfo], all_out: &[DeviceInfo]) -> Result<Resolved, String> {
    let allowed = |v: &[DeviceInfo]| -> Vec<DeviceInfo> {
        v.iter().filter(|d| !devices::is_denied(&d.name, deny)).cloned().collect()
    };
    let output = match &s.output_id {
        Some(id) => all_out
            .iter()
            .find(|d| &d.id == id)
            .cloned()
            .ok_or("O microfone virtual selecionado não está disponível")?,
        None => devices::find_virtual_mic(&allowed(all_out)).cloned().ok_or(NO_VIRTUAL_MIC)?,
    };
    let input = match &s.input_id {
        Some(id) => all_in
            .iter()
            .find(|d| &d.id == id)
            .cloned()
            .ok_or("O microfone selecionado não está disponível")?,
        None => devices::pick_real_mic(&allowed(all_in))
            .cloned()
            .ok_or("Nenhum microfone encontrado")?,
    };
    if is_cable_partner(&input, &output) {
        return Err(format!(
            "A entrada \"{}\" é o lado de captura do microfone virtual \"{}\": isso causaria eco em loop. Escolha o microfone real.",
            input.name, output.name
        ));
    }
    let monitor = if s.monitor_on { pick_monitor(s.monitor_id.as_deref(), deny, all_out, &output) } else { None };
    Ok(Resolved { input, output, monitor })
}

/// Digital outputs (S/PDIF, HDMI) usually have nothing plugged in: they come after analog ones.
fn is_digital_output(name: &str) -> bool {
    let n = name.to_lowercase();
    ["digital", "s/pdif", "spdif", "hdmi"].iter().any(|p| n.contains(p))
}

/// The "Ouvir meu microfone" device. An explicit id is honoured unless it is the cable we render into
/// (that would only mix the mic into the virtual mic twice) or has disappeared (no monitor, rather than
/// playing the mic through whatever else is there). With no choice it is the Windows default output,
/// never the cable or any other virtual device: if the default is virtual, the first real, non-denied
/// output (analog first).
fn pick_monitor(id: Option<&str>, deny: &[String], all_out: &[DeviceInfo], cable: &DeviceInfo) -> Option<DeviceInfo> {
    match id {
        Some(id) => all_out.iter().find(|d| d.id == id && d.id != cable.id).cloned(),
        None => {
            let usable = |d: &&DeviceInfo| d.id != cable.id && !d.is_virtual && !devices::is_denied(&d.name, deny);
            all_out
                .iter()
                .find(|d| d.is_default && usable(d))
                .or_else(|| all_out.iter().filter(usable).min_by_key(|d| is_digital_output(&d.name)))
                .cloned()
        }
    }
}

/// The device description in the last parentheses ("VB-Audio Virtual Cable"), else the whole name; lowercase.
fn device_group(name: &str) -> String {
    let inner = name.rfind('(').and_then(|open| name[open + 1..].strip_suffix(')'));
    inner.unwrap_or(name).trim().to_lowercase()
}

/// True when `input` is the capture side of the virtual cable that `output` renders into: feeding the
/// cable's own output back into its input would loop. Both endpoints of one virtual device share the
/// description in parentheses ("CABLE Input (VB-Audio Virtual Cable)" / "CABLE Output (VB-Audio Virtual Cable)").
fn is_cable_partner(input: &DeviceInfo, output: &DeviceInfo) -> bool {
    input.is_virtual && output.is_virtual && device_group(&input.name) == device_group(&output.name)
}

/// The capture endpoint that calls read as the virtual mic: the virtual input that shares its description
/// ("VB-Audio Virtual Cable") with the render endpoint `output_name` that Clarim writes into.
pub fn virtual_mic_capture<'a>(inputs: &'a [DeviceInfo], output_name: &str) -> Option<&'a DeviceInfo> {
    let group = device_group(output_name);
    inputs.iter().find(|d| d.is_virtual && device_group(&d.name) == group)
}

/// Id of the Windows default output for the Communications role (`comms`) or the Console role, which is
/// what apps without a choice of their own play on. The calling thread needs COM (`init_com`).
pub fn default_render_id(comms: bool) -> Option<String> {
    let role = if comms { Role::Communications } else { Role::Console };
    wasapi::get_default_device_for_role(&Direction::Render, &role).ok()?.get_id().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarim_core::is_virtual_device;

    fn dev(name: &str) -> DeviceInfo {
        DeviceInfo { id: format!("id-{name}"), name: name.into(), is_default: false, is_virtual: is_virtual_device(name) }
    }

    #[test]
    fn cable_capture_side_is_refused_as_input() {
        let out = dev("CABLE Input (VB-Audio Virtual Cable)");
        assert!(is_cable_partner(&dev("CABLE Output (VB-Audio Virtual Cable)"), &out));
        assert!(!is_cable_partner(&dev("Microfone (7- USB Audio Device)"), &out));
        // a second, unrelated virtual cable is a legitimate input
        assert!(!is_cable_partner(&dev("CABLE-A Output (VB-Audio Cable A)"), &out));
        let sonar = dev("SteelSeries Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)");
        assert!(is_cable_partner(&sonar, &sonar));
        assert!(is_cable_partner(&dev("Virtual Mic"), &dev("Virtual Mic")));
    }

    fn mic(name: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo { is_default, ..dev(name) }
    }

    #[test]
    fn picks_real_mic_and_cable_and_reports_missing_cable() {
        let ins = [mic("CABLE Output (VB-Audio Virtual Cable)", true), mic("Microfone (USB)", false)];
        let outs = [dev("Alto-falantes (Realtek)"), dev("CABLE Input (VB-Audio Virtual Cable)")];
        let r = pick(&MicSettings::default(), &[], &ins, &outs).expect("resolves");
        assert_eq!((r.input.name.as_str(), r.output.name.as_str()), ("Microfone (USB)", "CABLE Input (VB-Audio Virtual Cable)"));

        let err = pick(&MicSettings::default(), &[], &ins, &outs[..1]).err().expect("no cable");
        assert_eq!(err, NO_VIRTUAL_MIC);
        assert!(err.contains("vb-audio.com/Cable"));
    }

    #[test]
    fn explicit_choices_are_honoured_or_reported() {
        let ins = [mic("Microfone (USB)", false), mic("CABLE Output (VB-Audio Virtual Cable)", false)];
        let outs = [dev("CABLE Input (VB-Audio Virtual Cable)")];
        // explicit capture side of the same cable: refused instead of looping
        let s = MicSettings { input_id: Some(ins[1].id.clone()), ..MicSettings::default() };
        assert!(pick(&s, &[], &ins, &outs).err().is_some_and(|e| e.contains("eco em loop")));
        // explicit ids that are gone are reported, not silently replaced
        let s = MicSettings { input_id: Some("unplugged".into()), ..MicSettings::default() };
        assert!(pick(&s, &[], &ins, &outs).err().is_some_and(|e| e.contains("não está disponível")));
        let s = MicSettings { output_id: Some("gone".into()), ..MicSettings::default() };
        assert!(pick(&s, &[], &ins, &outs).err().is_some_and(|e| e.contains("virtual selecionado")));
        // the deny-list steers auto-pick only: a denied mic is still usable when chosen explicitly
        let deny = vec!["usb".to_string()];
        assert!(pick(&MicSettings::default(), &deny, &ins, &outs).err().is_some_and(|e| e.contains("Nenhum microfone")));
        let s = MicSettings { input_id: Some(ins[0].id.clone()), ..MicSettings::default() };
        assert!(pick(&s, &deny, &ins, &outs).is_ok());
    }

    #[test]
    fn monitor_defaults_to_the_windows_output_but_never_to_a_virtual_one() {
        let cable = dev("CABLE Input (VB-Audio Virtual Cable)");
        let speakers = mic("Alto-falantes (Realtek)", true);
        let hdmi = dev("LG HDMI (NVIDIA High Definition Audio)");
        let headset = dev("Fones (USB Audio)");
        let sonar = mic("SteelSeries Sonar - Gaming (SteelSeries Sonar Virtual Audio Device)", true);
        let on = |id: Option<&str>| MicSettings { monitor_on: true, monitor_id: id.map(String::from), ..MicSettings::default() };
        let pick_mon = |s: &MicSettings, outs: &[DeviceInfo]| {
            pick(s, &[], &[dev("Microfone (USB)")], outs).expect("resolves").monitor.map(|m| m.name)
        };

        // default = the Windows default output
        let outs = [cable.clone(), hdmi.clone(), speakers.clone(), headset.clone()];
        assert_eq!(pick_mon(&on(None), &outs).as_deref(), Some("Alto-falantes (Realtek)"));
        // default is virtual (Sonar): first real output, analog before HDMI, never the cable
        let outs = [cable.clone(), hdmi.clone(), sonar.clone(), headset.clone()];
        assert_eq!(pick_mon(&on(None), &outs).as_deref(), Some("Fones (USB Audio)"));
        // nothing real at all: no monitor, not the Sonar/cable endpoint
        assert_eq!(pick_mon(&on(None), &[cable.clone(), sonar.clone()]), None);
        // the deny-list applies to the automatic choice only
        let s = on(None);
        let denied = pick(&s, &["realtek".to_string()], &[dev("Microfone (USB)")], &[cable.clone(), speakers.clone(), headset.clone()]);
        assert_eq!(denied.expect("resolves").monitor.map(|m| m.name).as_deref(), Some("Fones (USB Audio)"));
        // an explicit choice is honoured, except the cable itself or a device that is gone
        let outs = [cable.clone(), speakers, headset.clone()];
        assert_eq!(pick_mon(&on(Some(&headset.id)), &outs).as_deref(), Some("Fones (USB Audio)"));
        assert_eq!(pick_mon(&on(Some(&cable.id)), &outs), None);
        assert_eq!(pick_mon(&on(Some("unplugged")), &outs), None);
        // off = no monitor thread whatever the device says
        let off = MicSettings { monitor_on: false, monitor_id: Some(headset.id.clone()), ..MicSettings::default() };
        assert_eq!(pick_mon(&off, &outs), None);
    }

    #[test]
    fn a_wanted_monitor_that_cannot_play_is_reported() {
        assert!(!monitor_down(false, false, false), "monitoring off: nothing to report");
        assert!(!monitor_down(false, true, true));
        assert!(!monitor_down(true, true, false), "playing");
        assert!(monitor_down(true, false, false), "no output device found for it");
        assert!(monitor_down(true, true, true), "its stream stopped (unplugged, busy)");
        assert!(MONITOR_DOWN.starts_with("Ouvir meu microfone"), "the dashboard matches this prefix");
        assert!(MIC_SILENT.starts_with("Microfone silenciado"), "the dashboard matches this prefix");
    }

    #[test]
    fn the_virtual_mic_capture_side_is_the_virtual_input_of_the_same_device() {
        let ins = [
            mic("Microfone (USB)", false),
            mic("CABLE-A Output (VB-Audio Cable A)", false),
            mic("CABLE Output (VB-Audio Virtual Cable)", false),
        ];
        let found = |out: &str| virtual_mic_capture(&ins, out).map(|d| d.name.as_str());
        assert_eq!(found("CABLE Input (VB-Audio Virtual Cable)"), Some("CABLE Output (VB-Audio Virtual Cable)"));
        assert_eq!(found("CABLE-A Input (VB-Audio Cable A)"), Some("CABLE-A Output (VB-Audio Cable A)"));
        assert_eq!(found("CABLE-B Input (VB-Audio Cable B)"), None, "another cable is not ours");
        // a real device that happens to share a description is not a virtual mic
        assert_eq!(found("Microfone (USB)"), None);
        assert!(virtual_mic_capture(&[], "CABLE Input (VB-Audio Virtual Cable)").is_none());
    }

    #[test]
    fn idle_engine_reports_nothing_running() {
        let mut engine = MicEngine::new(vec![]);
        let st = engine.status();
        assert!(!st.running && st.error.is_none() && st.denoise_active.is_none());
        assert!(engine.take_calibration().is_none());
        assert_eq!((st.references, st.echo_score, st.clip_events), (0, 0.0, 0));
        assert!(st.voice_db.is_none() && st.echo_db.is_none() && st.noise_db.is_none());
        assert!(engine.input_device_id().is_none() && engine.output_device_id().is_none() && engine.monitor_device_id().is_none());
        assert!(!engine.shift_mic_volume(-3.0), "no pipeline to tell");
        engine.set_references(&["whatever".to_string()]); // disabled / never applied: ignored, nothing is opened
        assert_eq!(engine.status().references, 0);
    }

    #[test]
    fn disabled_settings_stay_idle_without_touching_devices() {
        let mut engine = MicEngine::new(vec![]);
        engine.apply(&MicSettings { enabled: false, ..MicSettings::default() });
        let st = engine.status();
        assert!(!st.running && st.error.is_none());
        engine.start_calibration(5.0);
        assert!(engine.status().error.is_some_and(|e| e.contains("Calibração")));
        assert!(!engine.status().calibrating);
    }

    /// Manual check on the real machine (read-only enumeration, no stream is opened):
    /// `cargo test -p clarim-audio --lib -- --ignored resolve_on_this_machine --nocapture`.
    #[test]
    #[ignore = "enumerates the real audio devices"]
    fn resolve_on_this_machine() {
        let deny = clarim_core::default_deny_devices();
        match resolve(&MicSettings::default(), &deny) {
            Ok(r) => println!("auto: input '{}' -> virtual mic '{}'", r.input.name, r.output.name),
            Err(e) => println!("auto: {e}"),
        }
        // explicit virtual capture endpoint as input must be refused when it is the cable's own side
        let inputs = devices::enumerate(Flow::Capture, &[]).expect("inputs");
        let partner = devices::find_virtual_mic(&devices::enumerate(Flow::Render, &deny).expect("outputs")).map(|o| device_group(&o.name));
        if let Some(v) = inputs.iter().find(|d| d.is_virtual && Some(device_group(&d.name)) == partner) {
            let s = MicSettings { input_id: Some(v.id.clone()), ..MicSettings::default() };
            println!("explicit virtual input '{}': {:?}", v.name, resolve(&s, &deny).err());
        }
    }
}
