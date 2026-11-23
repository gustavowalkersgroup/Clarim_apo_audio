//! Serves the dashboard on http://127.0.0.1:47801/ with fake, animated data (no audio touched).
//!
//!   cargo run -p clarim-web --example mock [-- --no-vmic] [-- --error] [-- --no-routing] ...
//!
//! `--no-vmic`   : no virtual mic found (VB-Cable warning)
//! `--error`     : mic engine reports an error
//! `--no-routing`: per-app routing switched off at start
//! `--sonar`     : SteelSeries Sonar endpoints exist (and Sonar Gaming is the Windows default)
//! `--in-call`   : Chrome is using a microphone: it shows in Chat with the "em chamada" tag
//! `--silent-mic`: the mic runs but its input level is exactly 0 (the engine's "Microfone silenciado" note, one yellow "Sem sinal" banner, chip stays green)
//! `--monitor-note`: the engine reports that "Ouvir meu microfone" cannot play (yellow banner under Fones, chip stays green)
//! `--unrouted`  : Discord ignores its channel's output (the "não roteado" tag)
//! `--note`      : the engine reports a calibration note in `mic.error` (toast only, no red chip)
//! `--uncalibrated`: Voice Focus is on and the voice was never calibrated (the "sem calibrar" hint)
//! Output equalizer (default: Equalizer APO installed, written through the service, set up on the first real output).
//! The buttons work: "Instalar equalizador" runs a fake 6 s install (download, install, attach the channels' devices),
//! a device switch attaches / detaches, and either leaves "falta reiniciar o áudio" until "Reiniciar áudio agora" has run
//! (3 s). The flags only choose where it starts:
//! `--apo-missing`  : Equalizer APO is not installed (the card offers "Instalar equalizador")
//! `--apo-inactive` : installed, but set up on no playback device (the device switch is off)
//! `--apo-no-writer`: installed, but neither the Clarim service nor a writable config folder (error text, controls off)
//! `--apo-no-service`: not installed and the Clarim service is not there (the install button is off, the card says why)
//! `--apo-enh-off`  : Windows audio enhancements are off on the first real output (the APO cannot work there)
//! `--apo-unknown`  : the registry state of the first real output could not be read (no claim)
//! `--apo-busy`     : the service is installing Equalizer APO and never finishes (busy text, buttons disabled)
//! `--apo-needs-restart`: installed and attached, but Windows Audio has not restarted since ("Reiniciar áudio agora")
//! `--apo-install-error`: not installed, and the last install failed (the service's error text)

use anyhow::{anyhow, Result};
use clarim_core::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PORT: u16 = 47801;
const MSG_NO_WRITER: &str = "Instale o serviço do Clarim (clarim install) para usar o equalizador de saída";
const MSG_NO_SERVICE: &str = "Instale o serviço do Clarim (clarim install, como administrador) para o Clarim instalar e configurar o equalizador.";
// endpoint ids as Windows writes them: the GUID after "}." is what the service and the registry use
const RTK: &str = "{0.0.0.00000000}.{171dffe0-1f81-4558-a253-45945204afa2}";
const USB: &str = "{0.0.0.00000000}.{9fc5e635-237f-4f55-add4-030c65c6a5c9}";
const BT: &str = "{0.0.0.00000000}.{b9cc1e4e-11bd-4f54-8227-5e28f30a485d}";
const CABLE: &str = "CABLE Input (VB-Audio Virtual Cable)";
const SONAR_MIC: &str = "SteelSeries Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)";

/// Where the mock's Equalizer APO starts (the real agent asks the Clarim service over the pipe).
#[derive(Clone, Copy, PartialEq)]
enum Apo {
    Active,
    Missing,
    Inactive,
    NoWriter,
    NoService,
    EnhancementsOff,
    Unknown,
    Busy,
    NeedsRestart,
    InstallError,
}

#[derive(Clone, Copy)]
enum Op {
    Install,
    Restart,
}

/// What the buttons change: the pretend service's memory.
struct Sim {
    installed: bool,
    /// Endpoint ids Equalizer APO is attached to.
    configured: Vec<String>,
    needs_restart: bool,
    /// The running operation and when it started (one at a time, like the service).
    running: Option<(Op, Instant)>,
    error: Option<String>,
}

struct Proc {
    pid: u32,
    exe: &'static str,
    name: &'static str,
    /// Animation: base level, speed, phase.
    anim: (f32, f32, f32),
}

const INSTALL_TIME: Duration = Duration::from_secs(6);
const RESTART_TIME: Duration = Duration::from_secs(3);

const PROCS: [Proc; 6] = [
    Proc { pid: 4120, exe: "spotify.exe", name: "Spotify", anim: (0.55, 1.3, 0.0) },
    Proc { pid: 5532, exe: "discord.exe", name: "Discord", anim: (0.35, 3.1, 1.0) },
    Proc { pid: 7788, exe: "chrome.exe", name: "Google Chrome", anim: (0.30, 0.7, 2.0) },
    Proc { pid: 7791, exe: "chrome.exe", name: "Google Chrome", anim: (0.0, 0.0, 0.0) },
    Proc { pid: 9100, exe: "cs2.exe", name: "Counter-Strike 2", anim: (0.70, 5.0, 0.5) },
    Proc { pid: 3340, exe: "obs64.exe", name: "OBS Studio", anim: (0.12, 0.9, 4.0) },
];

struct Mock {
    start: Instant,
    cfg: Mutex<Config>,
    no_vmic: bool,
    error: bool,
    sonar: bool,
    in_call: bool,
    silent_mic: bool,
    unrouted: bool,
    note: bool,
    monitor_note: bool,
    apo: Apo,
    sim: Mutex<Sim>,
    calib_until: Mutex<Option<Instant>>,
}

fn dev(id: &str, name: &str, is_default: bool) -> DeviceInfo {
    DeviceInfo { id: id.into(), name: name.into(), is_default, is_virtual: is_virtual_device(name) }
}

impl Mock {
    fn inputs(&self) -> Vec<DeviceInfo> {
        let mut v = vec![
            dev("in-usb", "Microfone (7- USB Audio Device)", !self.sonar),
            dev("in-bt", "Headset Hands-Free (WZ-BT5.0)", false),
        ];
        if self.sonar {
            v.push(dev("in-sonar", SONAR_MIC, true));
        }
        v
    }

    /// Id of the Windows default playback device.
    fn default_out(&self) -> &'static str {
        if self.sonar {
            "out-sonar-game"
        } else {
            RTK
        }
    }

    fn outputs(&self) -> Vec<DeviceInfo> {
        let d = self.default_out();
        let mut v = vec![
            dev(RTK, "Alto-falantes (Realtek(R) Audio)", d == RTK),
            dev(USB, "Alto-falantes (7- USB Audio Device)", false),
            dev(BT, "Fones de ouvido (WZ-BT5.0)", false),
        ];
        if self.sonar {
            v.push(dev("out-sonar-game", "SteelSeries Sonar - Gaming (SteelSeries Sonar Virtual Audio Device)", true));
            v.push(dev("out-sonar-mic", SONAR_MIC, false));
        }
        if !self.no_vmic {
            v.push(dev("out-cable", CABLE, false));
        }
        v
    }

    /// The virtual mic the engine would use: VB-Cable first, Sonar's microphone as the fallback.
    fn vmic(&self) -> Option<&'static str> {
        match (self.no_vmic, self.sonar) {
            (false, _) => Some(CABLE),
            (true, true) => Some(SONAR_MIC),
            (true, false) => None,
        }
    }

    /// The physical devices the channels play on (what "Instalar" attaches), by endpoint id.
    fn channel_devices(&self, cfg: &Config) -> Vec<String> {
        let outs = self.outputs();
        let mut v: Vec<String> = Vec::new();
        for ch in ChannelId::ALL {
            let want = match &cfg.channels[&ch].device_id {
                Some(id) if outs.iter().any(|d| &d.id == id) => id.clone(),
                _ => self.default_out().to_string(),
            };
            if outs.iter().any(|d| d.id == want && !d.is_virtual) && !v.contains(&want) {
                v.push(want);
            }
        }
        v
    }

    /// Finishes an operation whose time is up (the real service does it on a worker thread).
    fn apo_tick(&self) {
        let targets = self.channel_devices(&self.cfg.lock().unwrap());
        let mut sim = self.sim.lock().unwrap();
        match sim.running {
            Some((Op::Install, at)) if at.elapsed() >= INSTALL_TIME => {
                sim.running = None;
                sim.installed = true;
                for id in targets {
                    if !sim.configured.contains(&id) {
                        sim.configured.push(id);
                    }
                }
                sim.needs_restart = true;
            }
            Some((Op::Restart, at)) if at.elapsed() >= RESTART_TIME => {
                sim.running = None;
                sim.needs_restart = false;
            }
            _ => {}
        }
    }

    fn eq_apo(&self) -> EqApoStatus {
        self.apo_tick();
        let sim = self.sim.lock().unwrap();
        let first: Vec<String> = self.outputs().into_iter().filter(|d| !d.is_virtual).take(1).map(|d| d.id).collect();
        let mut st = EqApoStatus {
            installed: sim.installed,
            writer: "service".into(),
            needs_restart: sim.needs_restart,
            error: sim.error.clone(),
            ..EqApoStatus::default()
        };
        st.busy = match sim.running {
            Some((Op::Install, at)) if !sim.installed => Some(if at.elapsed() < INSTALL_TIME / 3 {
                "Baixando o Equalizer APO..."
            } else if at.elapsed() < INSTALL_TIME * 2 / 3 {
                "Instalando o Equalizer APO..."
            } else {
                "Configurando os aparelhos..."
            }),
            Some((Op::Install, _)) => Some("Configurando os aparelhos..."),
            Some((Op::Restart, _)) => Some("Reiniciando o áudio do Windows..."),
            None if self.apo == Apo::Busy => Some("Instalando o Equalizer APO..."),
            None => None,
        }
        .map(Into::into);
        if sim.installed {
            st.active_devices = sim.configured.clone();
        }
        match self.apo {
            Apo::NoWriter => {
                st.writer = "none".into();
                st.error = Some(MSG_NO_WRITER.into());
            }
            Apo::NoService => {
                st.writer = "none".into();
                st.error = Some(MSG_NO_SERVICE.into());
            }
            Apo::EnhancementsOff => st.enhancements_off = first,
            Apo::Unknown => {
                st.active_devices.clear();
                st.unknown_devices = first;
            }
            _ => {}
        }
        // what the service lists: every real endpoint with its registry state, keyed by GUID
        st.devices = self
            .outputs()
            .into_iter()
            .filter_map(|d| {
                let guid = endpoint_guid(&d.id)?.to_string();
                Some(EqApoDevice {
                    configured: sim.installed && sim.configured.contains(&d.id),
                    can_configure: !d.is_virtual,
                    enhancements_off: st.enhancements_off.contains(&d.id),
                    guid,
                })
            })
            .collect();
        st
    }

    /// The three service commands share these refusals (the real ones come from the service, in PT-BR).
    fn apo_command(&self, f: impl FnOnce(&mut Sim) -> Result<()>) -> Result<()> {
        if matches!(self.apo, Apo::NoWriter | Apo::NoService) {
            return Err(anyhow!(MSG_NO_SERVICE));
        }
        self.apo_tick();
        let mut sim = self.sim.lock().unwrap();
        if self.apo == Apo::Busy || sim.running.is_some() {
            return Err(anyhow!("Já há uma operação em andamento: o Clarim está mexendo no equalizador."));
        }
        sim.error = None;
        f(&mut sim)
    }

    fn t(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    fn in_call(&self, p: &Proc) -> bool {
        self.in_call && p.exe == "chrome.exe"
    }

    /// The channel the app really plays on: its rule, but Chat while a Media app is in a call.
    fn effective(&self, cfg: &Config, p: &Proc) -> ChannelId {
        match cfg.channel_for(p.exe) {
            ChannelId::Media if self.in_call(p) && cfg.call_to_chat => ChannelId::Chat,
            ch => ch,
        }
    }

    /// Where the session lives: on its channel's device (or the Windows default), unless it ignores the route.
    fn session_device(&self, cfg: &Config, p: &Proc) -> String {
        if self.unrouted && p.exe == "discord.exe" {
            return RTK.into();
        }
        let outs = self.outputs();
        match &cfg.channels[&self.effective(cfg, p)].device_id {
            Some(id) if outs.iter().any(|d| &d.id == id) => id.clone(),
            _ => self.default_out().into(),
        }
    }

    fn calibrating(&self) -> Option<f32> {
        let mut g = self.calib_until.lock().unwrap();
        match *g {
            Some(end) if Instant::now() < end => {
                Some(1.0 - end.saturating_duration_since(Instant::now()).as_secs_f32() / 5.0)
            }
            Some(_) => {
                *g = None;
                self.cfg.lock().unwrap().mic.voice_level_db = Some(-23.4);
                None
            }
            None => None,
        }
    }

    fn mic_status(&self) -> MicStatus {
        let c = self.cfg.lock().unwrap().mic.clone();
        let cal = self.calibrating();
        if self.error {
            return MicStatus {
                running: false,
                error: Some("Não foi possível abrir o microfone: dispositivo em uso por outro programa.".into()),
                ..MicStatus::default()
            };
        }
        let running = c.enabled;
        let t = self.t();
        let speaking = (t * 0.8).sin() > -0.2 && !self.silent_mic;
        let env = if speaking { 0.25 + 0.2 * (t * 7.0).sin().abs() } else { 0.01 };
        let gain = 10f32.powf(c.input_gain_db / 20.0);
        let denoise_active = match c.denoise {
            DenoiseMode::Strong if self.no_vmic => Some(DenoiseMode::Light),
            d => Some(d),
        };
        let in_peak = if !running || self.silent_mic { 0.0 } else { (env * gain + 0.02).min(1.0) };
        MicStatus {
            running,
            error: if self.note {
                Some("Calibração falhou: fale mais alto/perto do microfone".to_string())
            } else if self.silent_mic {
                // what the engine says after 2 s of silence-flagged packets
                Some("Microfone silenciado ou bloqueado pelo Windows (Configurações > Privacidade > Microfone)".to_string())
            } else if self.monitor_note {
                Some("Ouvir meu microfone indisponível: a saída escolhida não foi encontrada ou parou (fones desconectados?). Desligue e ligue a opção para tentar de novo.".to_string())
            } else {
                None
            },
            input_name: Some("Microfone (7- USB Audio Device)".into()),
            output_name: self.vmic().map(Into::into),
            denoise_active,
            in_peak,
            out_peak: if running && speaking { (env * 0.8 * 10f32.powf(c.output_gain_db / 20.0)).min(1.0) } else { 0.0 },
            vad: if running && speaking { 0.9 } else { 0.05 },
            focus_open: running && speaking,
            dsp_us: 640 + ((t * 3.0).sin() * 60.0) as u32,
            latency_ms: 34.0,
            calibrating: cal.is_some(),
            calibration_progress: cal.unwrap_or(0.0),
        }
    }

    fn peak(&self, p: &Proc) -> f32 {
        let (base, speed, phase) = p.anim;
        let cfg = self.cfg.lock().unwrap();
        let ch = &cfg.channels[&self.effective(&cfg, p)];
        let level = if ch.muted { 0.0 } else { ch.volume };
        (base * (0.5 + 0.5 * (self.t() * speed + phase).sin().abs()) * level).clamp(0.0, 1.0)
    }
}

impl Backend for Mock {
    fn snapshot(&self) -> Snapshot {
        let cfg = self.cfg.lock().unwrap().clone();
        let sessions = PROCS
            .iter()
            .map(|p| AppSession {
                pid: p.pid,
                exe: p.exe.into(),
                name: p.name.into(),
                channel: self.effective(&cfg, p),
                has_rule: cfg.app_rules.contains_key(p.exe),
                in_call: self.in_call(p) && cfg.call_to_chat,
                device_id: self.session_device(&cfg, p),
                active: p.pid != 7791,
                peak: self.peak(p),
            })
            .collect();
        Snapshot {
            version: "mock".into(),
            config: cfg,
            inputs: self.inputs(),
            outputs: self.outputs(),
            sessions,
            mic: self.mic_status(),
            virtual_mic_found: self.vmic().is_some(),
            eq_apo: self.eq_apo(),
        }
    }

    fn meters(&self) -> Meters {
        Meters { mic: self.mic_status(), sessions: PROCS.iter().map(|p| (p.pid, self.peak(p))).collect() }
    }

    fn set_mic(&self, s: MicSettings) -> Result<()> {
        eprintln!("set_mic {}", serde_json::to_string(&s)?);
        self.cfg.lock().unwrap().mic = s;
        Ok(())
    }

    fn set_channel(&self, id: ChannelId, s: ChannelSettings) -> Result<()> {
        eprintln!("set_channel {id:?} {s:?}");
        self.cfg.lock().unwrap().channels.insert(id, s);
        Ok(())
    }

    fn assign_app(&self, exe: &str, ch: Option<ChannelId>) -> Result<()> {
        eprintln!("assign {exe} {ch:?}");
        let mut c = self.cfg.lock().unwrap();
        match ch {
            Some(ch) => c.app_rules.insert(exe.into(), ch),
            None => c.app_rules.remove(exe),
        };
        Ok(())
    }

    fn set_routing_enabled(&self, on: bool) -> Result<()> {
        eprintln!("routing {on}");
        self.cfg.lock().unwrap().routing_enabled = on;
        Ok(())
    }

    fn start_calibration(&self, seconds: f32) -> Result<()> {
        eprintln!("calibrate {seconds}");
        if !self.cfg.lock().unwrap().mic.enabled {
            return Err(anyhow!("O microfone está desligado."));
        }
        *self.calib_until.lock().unwrap() = Some(Instant::now() + std::time::Duration::from_secs_f32(seconds));
        Ok(())
    }

    fn set_output_eq(&self, device_id: &str, eq: Option<OutputEq>) -> Result<()> {
        eprintln!("output_eq {device_id} {eq:?}");
        if !self.sim.lock().unwrap().installed {
            return Err(anyhow!("O Equalizer APO não está instalado."));
        }
        if self.apo == Apo::NoWriter {
            return Err(anyhow!(MSG_NO_WRITER));
        }
        let mut c = self.cfg.lock().unwrap();
        match eq {
            Some(eq) => c.output_eq.insert(device_id.into(), eq),
            None => c.output_eq.remove(device_id),
        };
        Ok(())
    }

    fn install_eq_apo(&self) -> Result<()> {
        eprintln!("install equalizer");
        self.apo_command(|sim| {
            sim.running = Some((Op::Install, Instant::now()));
            Ok(())
        })
    }

    fn set_eq_device(&self, device_id: &str, enabled: bool) -> Result<()> {
        eprintln!("eq device {device_id} {enabled}");
        let virt = self.outputs().iter().find(|d| d.id == device_id).map(|d| d.is_virtual);
        self.apo_command(|sim| {
            if enabled {
                if !sim.installed {
                    return Err(anyhow!("O Equalizer APO não está instalado."));
                }
                if virt != Some(false) {
                    return Err(anyhow!("Este aparelho não aceita o equalizador (é um dispositivo virtual)."));
                }
                if !sim.configured.iter().any(|c| c == device_id) {
                    sim.configured.push(device_id.to_string());
                    sim.needs_restart = true;
                }
            } else if sim.configured.iter().any(|c| c == device_id) {
                sim.configured.retain(|c| c != device_id);
                sim.needs_restart = true;
            }
            Ok(())
        })
    }

    fn restart_audio(&self) -> Result<()> {
        eprintln!("restart audio");
        self.apo_command(|sim| {
            sim.running = Some((Op::Restart, Instant::now()));
            Ok(())
        })
    }

    fn refresh_devices(&self) {
        eprintln!("refresh");
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let mut cfg = Config { routing_enabled: !flag("--no-routing"), ..Config::default() };
    cfg.channels.entry(ChannelId::Game).or_default().device_id = Some(RTK.into());
    cfg.channels.entry(ChannelId::Chat).or_default().device_id = Some(BT.into());
    cfg.channels.entry(ChannelId::Media).or_default().volume = 0.6;
    cfg.channels.entry(ChannelId::Aux).or_default().device_id = Some("out-gone".into()); // unplugged device
    cfg.mic.voice_level_db = if flag("--uncalibrated") { None } else { Some(-24.0) };
    cfg.mic.voice_focus = flag("--uncalibrated");
    let apo = [
        ("--apo-missing", Apo::Missing),
        ("--apo-inactive", Apo::Inactive),
        ("--apo-no-writer", Apo::NoWriter),
        ("--apo-no-service", Apo::NoService),
        ("--apo-enh-off", Apo::EnhancementsOff),
        ("--apo-unknown", Apo::Unknown),
        ("--apo-busy", Apo::Busy),
        ("--apo-needs-restart", Apo::NeedsRestart),
        ("--apo-install-error", Apo::InstallError),
    ]
    .into_iter()
    .find_map(|(name, apo)| flag(name).then_some(apo))
    .unwrap_or(Apo::Active);
    let installed = !matches!(apo, Apo::Missing | Apo::NoService | Apo::Busy | Apo::InstallError);
    let attached = matches!(apo, Apo::Active | Apo::NoWriter | Apo::EnhancementsOff | Apo::NeedsRestart);
    let sim = Sim {
        installed,
        configured: if attached { vec![RTK.into()] } else { vec![] },
        needs_restart: apo == Apo::NeedsRestart,
        running: None,
        error: (apo == Apo::InstallError)
            .then(|| "Não foi possível baixar o Equalizer APO. Confira a conexão com a internet e tente de novo.".to_string()),
    };
    let mock = Mock {
        start: Instant::now(),
        cfg: Mutex::new(cfg),
        no_vmic: flag("--no-vmic"),
        error: flag("--error"),
        sonar: flag("--sonar"),
        in_call: flag("--in-call"),
        silent_mic: flag("--silent-mic"),
        unrouted: flag("--unrouted"),
        note: flag("--note"),
        monitor_note: flag("--monitor-note"),
        apo,
        sim: Mutex::new(sim),
        calib_until: Mutex::new(None),
    };
    let handle = clarim_web::serve(std::sync::Arc::new(mock), PORT)?;
    eprintln!("mock dashboard on http://127.0.0.1:{PORT}/");
    let _ = handle.join();
    Ok(())
}
