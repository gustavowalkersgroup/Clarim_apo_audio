//! Shared types, config persistence and the `Backend` trait for Clarim.
//! No Windows dependencies: everything here builds and tests anywhere.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 47800;
pub const APP_NAME: &str = "Clarim";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ChannelId {
    Game,
    Chat,
    Media,
    Aux,
}

impl ChannelId {
    pub const ALL: [ChannelId; 4] = [ChannelId::Game, ChannelId::Chat, ChannelId::Media, ChannelId::Aux];

    pub fn label(self) -> &'static str {
        match self {
            ChannelId::Game => "Jogo",
            ChannelId::Chat => "Chat",
            ChannelId::Media => "Mídia",
            ChannelId::Aux => "Aux",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DenoiseMode {
    Off,
    Light,
    Strong,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum EqPreset {
    Flat,
    Clarity,
    Warm,
    Broadcast,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct MicSettings {
    pub enabled: bool,
    /// None = auto-pick real mic.
    pub input_id: Option<String>,
    /// None = auto-pick virtual cable render endpoint.
    pub output_id: Option<String>,
    /// Headphones for self-listening.
    pub monitor_id: Option<String>,
    pub monitor_on: bool,
    pub denoise: DenoiseMode,
    /// 0..1. RNNoise: wet mix. DFN: attenuation limit (0 -> 6 dB ... 1 -> 100 dB, log-ish).
    pub denoise_strength: f32,
    pub voice_focus: bool,
    /// -40..0, threshold relative to the calibrated voice level.
    pub voice_focus_db: f32,
    /// Calibrated speech RMS in dBFS; None = not calibrated (assume -26 dBFS).
    pub voice_level_db: Option<f32>,
    pub gate_on: bool,
    pub gate_threshold_db: f32,
    /// 80 Hz high-pass.
    pub hpf_on: bool,
    /// Gentle compressor + makeup toward ~-18 dBFS speech.
    pub auto_level: bool,
    pub eq: EqPreset,
    /// Tone sliders applied after the `eq` preset: low shelf / mid peak / high shelf, each
    /// `-EQ_TONE_RANGE_DB..=EQ_TONE_RANGE_DB` dB. 0 = untouched.
    pub eq_bass_db: f32,
    pub eq_mid_db: f32,
    pub eq_treble_db: f32,
    /// -20..+20
    pub input_gain_db: f32,
    /// -20..+20
    pub output_gain_db: f32,
}

impl Default for MicSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            input_id: None,
            output_id: None,
            monitor_id: None,
            monitor_on: false,
            denoise: DenoiseMode::Strong,
            denoise_strength: 1.0,
            voice_focus: false,
            voice_focus_db: -14.0,
            voice_level_db: None,
            gate_on: false,
            gate_threshold_db: -50.0,
            hpf_on: true,
            auto_level: true,
            eq: EqPreset::Flat,
            eq_bass_db: 0.0,
            eq_mid_db: 0.0,
            eq_treble_db: 0.0,
            input_gain_db: 0.0,
            output_gain_db: 0.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ChannelSettings {
    /// None = Windows default render device.
    pub device_id: Option<String>,
    /// 0..1
    pub volume: f32,
    pub muted: bool,
}

impl Default for ChannelSettings {
    fn default() -> Self {
        Self { device_id: None, volume: 1.0, muted: false }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub mic: MicSettings,
    /// Always all 4 keys after `load()`.
    pub channels: BTreeMap<ChannelId, ChannelSettings>,
    /// Key = lowercase exe file name, e.g. "spotify.exe".
    pub app_rules: BTreeMap<String, ChannelId>,
    /// Channel for apps without a rule.
    pub default_channel: ChannelId,
    /// false = observe only, never touch app routing/volume.
    pub routing_enabled: bool,
    /// Case-insensitive substrings hidden from device lists.
    pub deny_devices: Vec<String>,
    pub port: u16,
    /// Midia apps that start using a microphone (a call in the browser: Meet, Teams web, Discord web)
    /// play on Chat until 20 s after the call.
    #[serde(default = "default_true")]
    pub call_to_chat: bool,
    /// Output equalizer per playback device (key = endpoint id), written as an Equalizer APO config.
    /// A missing entry means "no EQ" (same as all zeros).
    pub output_eq: BTreeMap<String, OutputEq>,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            mic: MicSettings::default(),
            channels: ChannelId::ALL.iter().map(|&c| (c, ChannelSettings::default())).collect(),
            app_rules: default_app_rules(),
            default_channel: ChannelId::Game,
            routing_enabled: true,
            deny_devices: default_deny_devices(),
            port: DEFAULT_PORT,
            call_to_chat: true,
            output_eq: BTreeMap::new(),
        }
    }
}

impl Config {
    /// `%APPDATA%\Clarim\config.json`.
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME)
            .join("config.json")
    }

    /// Missing or corrupt file gives defaults (a corrupt file is renamed to `config.json.bad`).
    pub fn load() -> Config {
        Self::load_from(&Self::path())
    }

    /// Atomic: writes `config.json.tmp`, then renames over `config.json`.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// Rule lookup, case-insensitive, else `default_channel`.
    pub fn channel_for(&self, exe: &str) -> ChannelId {
        self.app_rules
            .get(&exe.to_lowercase())
            .copied()
            .unwrap_or(self.default_channel)
    }

    fn load_from(path: &Path) -> Config {
        let parsed = match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str::<Config>(&t).map_err(|e| e.to_string()),
            // Not valid UTF-8: as unusable as bad JSON.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(e.to_string()),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("config: cannot read {}: {e}", path.display());
                }
                return Config::default();
            }
        };
        match parsed {
            Ok(mut cfg) => {
                cfg.normalize();
                cfg
            }
            Err(e) => {
                let bad = with_suffix(path, ".bad");
                log::warn!("config: {} is corrupt ({e}); moving to {}", path.display(), bad.display());
                if let Err(e) = std::fs::rename(path, &bad) {
                    log::warn!("config: rename to .bad failed: {e}");
                }
                Config::default()
            }
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let tmp = with_suffix(path, ".tmp");
        let json = serde_json::to_string_pretty(self).context("serialize config")?;
        // Flush to disk before the rename so a crash/power loss never leaves a truncated config.json.
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(json.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("sync {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    /// Fill missing channels and lowercase rule keys (hand-edited files).
    fn normalize(&mut self) {
        // Privileged ports are never a valid dashboard port for a per-user agent.
        if self.port < 1024 {
            self.port = DEFAULT_PORT;
        }
        for c in ChannelId::ALL {
            self.channels.entry(c).or_default();
        }
        if self.app_rules.keys().any(|k| *k != k.to_lowercase()) {
            self.app_rules = std::mem::take(&mut self.app_rules)
                .into_iter()
                .map(|(k, v)| (k.to_lowercase(), v))
                .collect();
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn default_app_rules() -> BTreeMap<String, ChannelId> {
    const CHAT: &[&str] = &[
        "discord.exe", "teams.exe", "ms-teams.exe", "zoom.exe", "slack.exe", "whatsapp.exe",
        "telegram.exe", "skype.exe", "signal.exe", "mumble.exe", "teamspeak.exe", "ts3client_win64.exe",
        "discordptb.exe", "discordcanary.exe", "whatsapp.root.exe",
    ];
    const MEDIA: &[&str] = &[
        "spotify.exe", "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe", "vlc.exe",
        "foobar2000.exe", "musicbee.exe", "aimp.exe", "itunes.exe", "applemusic.exe", "deezer.exe",
        "tidal.exe", "mpc-hc64.exe", "potplayermini64.exe",
    ];
    const AUX: &[&str] = &["obs64.exe"];
    let mut m = BTreeMap::new();
    for (list, ch) in [(CHAT, ChannelId::Chat), (MEDIA, ChannelId::Media), (AUX, ChannelId::Aux)] {
        for exe in list {
            m.insert((*exe).to_string(), ch);
        }
    }
    m
}

pub fn default_deny_devices() -> Vec<String> {
    vec!["NVIDIA High Definition Audio".to_string(), "NVIDIA Broadcast".to_string()]
}

/// Heuristic: endpoint is a virtual/loopback device, not real hardware.
pub fn is_virtual_device(name: &str) -> bool {
    const PATTERNS: [&str; 8] = [
        "cable", "sonar", "virtual", "steam streaming", "nvidia broadcast", "voicemeeter",
        "stereo mix", "mixagem estéreo",
    ];
    let n = name.to_lowercase();
    PATTERNS.iter().any(|p| n.contains(p))
}

/// Range of every tone slider (mic and output equalizer): `-12..=12` dB.
pub const EQ_TONE_RANGE_DB: f32 = 12.0;

/// Output tone controls of one playback device: low shelf, mid peak and high shelf gains in dB.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct OutputEq {
    pub bass_db: f32,
    pub mid_db: f32,
    pub treble_db: f32,
}

impl OutputEq {
    /// Gains clamped to `+-EQ_TONE_RANGE_DB`; NaN/inf is refused.
    pub fn clamped(self) -> Result<OutputEq> {
        let gains = [self.bass_db, self.mid_db, self.treble_db];
        if !gains.iter().all(|g| g.is_finite()) {
            bail!("Ganho do equalizador inválido.");
        }
        let [bass_db, mid_db, treble_db] = gains.map(|g| g.clamp(-EQ_TONE_RANGE_DB, EQ_TONE_RANGE_DB));
        Ok(OutputEq { bass_db, mid_db, treble_db })
    }
}

/// State of Equalizer APO for the output EQ, filled by the agent.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct EqApoStatus {
    /// `EqualizerAPO.dll` exists in `<Program Files>\EqualizerAPO`. With the service answering, a complete
    /// install: the DLL plus `HKLM\SOFTWARE\EqualizerAPO` plus both classes registered.
    pub installed: bool,
    /// Who can write the APO config: `"service"` (Clarim service over the pipe), `"direct"` (the
    /// config folder is writable by the user) or `"none"` (the service must be installed).
    pub writer: String,
    /// Render endpoint ids where APO is installed (registry hint, unverified).
    pub active_devices: Vec<String>,
    /// Render endpoint ids whose APO state could not be read.
    pub unknown_devices: Vec<String>,
    /// Render endpoint ids with Windows "audio enhancements" switched off (APO does not run there).
    pub enhancements_off: Vec<String>,
    /// PT-BR text of the last write failure, if any.
    pub error: Option<String>,
    /// PT-BR text while the Clarim service installs Equalizer APO or restarts Windows Audio (a long
    /// operation is running; the service takes one at a time), `None` otherwise.
    pub busy: Option<String>,
    /// A device was configured (or unconfigured) after the running Windows audio graph (`audiodg.exe`)
    /// started, so the change has no effect until Windows Audio restarts.
    pub needs_restart: bool,
    /// `Some(true)` = a running `audiodg.exe` has `EqualizerAPO.dll` loaded, `Some(false)` = it does not,
    /// `None` = the service cannot tell (no `audiodg.exe` running, or it refuses to be inspected).
    pub apo_loaded: Option<bool>,
    /// What the service knows about every playback endpoint (registry state), keyed by endpoint GUID.
    pub devices: Vec<EqApoDevice>,
}

impl Default for EqApoStatus {
    fn default() -> Self {
        Self {
            installed: false,
            writer: "none".to_string(),
            active_devices: Vec::new(),
            unknown_devices: Vec::new(),
            enhancements_off: Vec::new(),
            error: None,
            busy: None,
            needs_restart: false,
            apo_loaded: None,
            devices: Vec::new(),
        }
    }
}

/// One playback endpoint as the Clarim service sees it (registry only, no audio call).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct EqApoDevice {
    /// `"{8-4-4-4-12}"`, the part of the endpoint id after `"}."`, as named in the registry.
    pub guid: String,
    /// Equalizer APO is attached to this endpoint (any of its effect slots holds an APO class id).
    pub configured: bool,
    /// Clarim may configure it: not a virtual device (cable, Sonar, Voicemeeter...) and present.
    pub can_configure: bool,
    /// Windows "audio enhancements" are switched off here (APO does not run there).
    pub enhancements_off: bool,
}

/// `"{0.0.0.00000000}.{GUID}"` (IMMDevice id) -> `"{GUID}"`: the part after the first `"}."`.
/// `None` unless that part is a well-formed braced GUID, so a `Some` is always safe to hand to
/// `render_eqapo_file` or to put in a registry path.
pub fn endpoint_guid(endpoint_id: &str) -> Option<&str> {
    let (_, guid) = endpoint_id.split_once("}.")?;
    is_braced_guid(guid).then_some(guid)
}

/// `^\{[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}\}$`, byte for byte
/// (no trailing newline allowed, which a `$` in some regex flavours would let through).
pub fn is_braced_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 38
        && b.iter().enumerate().all(|(i, c)| match i {
            0 => *c == b'{',
            37 => *c == b'}',
            9 | 14 | 19 | 24 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The `Device:` line that scopes the filters after it to one endpoint. UNVERIFIED: APO matches the
/// pattern against device name, connection name and GUID, but whether the braced GUID matches is
/// untested (docs/eqapo.md); change the format only here if a live test says otherwise.
fn device_line(guid: &str) -> String {
    format!("Device: {guid}")
}

/// A gain in tenths of a dB as `-1.5` (one decimal, `.` separator, never `-0.0`).
fn tenths_db(t: i32) -> String {
    format!("{}{}.{}", if t < 0 { "-" } else { "" }, t.abs() / 10, t.abs() % 10)
}

/// The whole `clarim-eq.txt` for Equalizer APO, from `(endpoint GUID "{...}", EQ)` pairs.
///
/// The only producer of that file: GUIDs are checked against the strict braced-GUID shape and gains
/// are numbers, so nothing a client sends can add a `VSTPlugin:`/`Include:` line (the Windows audio
/// engine loads this file). Gains are clamped to `+-EQ_TONE_RANGE_DB` (NaN/inf is an error) and
/// rounded to 0.1 dB; a device whose gains are all zero gets no block. Each block is
/// `Device:` + `Preamp:` (minus the largest positive gain) + low shelf 105 Hz, peak 1 kHz, high shelf
/// 8 kHz. The trailing `Device: all` keeps anything after our `Include:` from being scoped to the
/// last device.
pub fn render_eqapo_file(entries: &[(String, OutputEq)]) -> Result<String> {
    let mut out = String::from("# Clarim output equalizer - generated, do not edit\n");
    for (guid, eq) in entries {
        if !is_braced_guid(guid) {
            bail!("Identificador de dispositivo inválido.");
        }
        let eq = eq.clamped()?;
        let [bass, mid, treble] = [eq.bass_db, eq.mid_db, eq.treble_db].map(|g| (g * 10.0).round() as i32);
        if bass == 0 && mid == 0 && treble == 0 {
            continue;
        }
        let preamp = -bass.max(mid).max(treble).max(0);
        out += &format!(
            "{}\nPreamp: {} dB\n\
             Filter 1: ON LSC Fc 105 Hz Gain {} dB Q 0.71\n\
             Filter 2: ON PK Fc 1000 Hz Gain {} dB Q 1.0\n\
             Filter 3: ON HSC Fc 8000 Hz Gain {} dB Q 0.71\n",
            device_line(guid),
            tenths_db(preamp),
            tenths_db(bass),
            tenths_db(mid),
            tenths_db(treble),
        );
    }
    out += "Device: all\n";
    Ok(out)
}

/// The line Clarim keeps in Equalizer APO's `config.txt` to pull in `clarim-eq.txt`.
pub const EQAPO_INCLUDE_LINE: &str = "Include: clarim-eq.txt";

/// The `config.txt` that Equalizer APO 1.4.2's installer ships (read from a real install): a -6 dB preamp,
/// the "simple bass boost" example and a flat GraphicEQ. APO applies it to EVERY device it is attached to.
const APO_STOCK_CONFIG: [&str; 3] = [
    "Preamp: -6 dB",
    "Include: example.txt",
    "GraphicEQ: 25 0; 40 0; 63 0; 100 0; 160 0; 250 0; 400 0; 630 0; 1000 0; 1600 0; 2500 0; 4000 0; 6300 0; 10000 0; 16000 0",
];

/// Is this `config.txt` text exactly Equalizer APO's stock default (`APO_STOCK_CONFIG`)? Compared line by line
/// after dropping a UTF-8 BOM, trimming, collapsing inner whitespace, ignoring empty lines and case. The stock
/// lines with Clarim's own `Include: clarim-eq.txt` appended still count; any other change (a different value,
/// an extra line) means the user customised it and this says false. The one rule every writer uses to decide
/// whether `config.txt` may be replaced by just the Include line (the stock one lowers the volume by 6 dB).
pub fn is_stock_apo_config(text: &str) -> bool {
    let norm = |l: &str| l.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    let lines: Vec<String> = text.trim_start_matches('\u{feff}').lines().map(norm).filter(|l| !l.is_empty()).collect();
    let stock = APO_STOCK_CONFIG.map(norm);
    lines == stock || (lines.len() == 4 && lines[..3] == stock && lines[3] == norm(EQAPO_INCLUDE_LINE))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub is_virtual: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppSession {
    pub pid: u32,
    /// Lowercase file name.
    pub exe: String,
    /// Session display name, else exe without `.exe`.
    pub name: String,
    /// EFFECTIVE channel: resolved via `Config::channel_for`, except Chat while `in_call`.
    pub channel: ChannelId,
    /// Exe present in `app_rules`.
    pub has_rule: bool,
    /// True while this Midia app captures a microphone and is therefore played on Chat.
    #[serde(default)]
    pub in_call: bool,
    /// Endpoint the session currently lives on.
    pub device_id: String,
    /// AudioSessionStateActive.
    pub active: bool,
    /// 0..1
    pub peak: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MicStatus {
    pub running: bool,
    /// Human-readable PT-BR when possible.
    pub error: Option<String>,
    pub input_name: Option<String>,
    /// Virtual mic actually in use.
    pub output_name: Option<String>,
    /// What is really running (Strong may fall back to Light).
    pub denoise_active: Option<DenoiseMode>,
    /// 0..1
    pub in_peak: f32,
    /// 0..1
    pub out_peak: f32,
    /// 0..1 speech probability.
    pub vad: f32,
    /// Voice focus currently passing audio.
    pub focus_open: bool,
    pub dsp_us: u32,
    pub latency_ms: f32,
    pub calibrating: bool,
    /// 0..1
    pub calibration_progress: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meters {
    pub mic: MicStatus,
    /// (pid, peak)
    pub sessions: Vec<(u32, f32)>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub version: String,
    pub config: Config,
    /// Deny-list filtered.
    pub inputs: Vec<DeviceInfo>,
    /// Deny-list filtered (virtual ones included, flagged).
    pub outputs: Vec<DeviceInfo>,
    pub sessions: Vec<AppSession>,
    pub mic: MicStatus,
    pub virtual_mic_found: bool,
    /// Equalizer APO state for the output EQ.
    #[serde(default)]
    pub eq_apo: EqApoStatus,
}

/// Implemented by apps/clarim; consumed by clarim-web. All methods are cheap and non-blocking
/// (heavy work happens on engine threads). Errors are user-facing strings (PT-BR ok).
pub trait Backend: Send + Sync + 'static {
    fn snapshot(&self) -> Snapshot;
    fn meters(&self) -> Meters;
    /// Persists + applies live.
    fn set_mic(&self, s: MicSettings) -> Result<()>;
    fn set_channel(&self, id: ChannelId, s: ChannelSettings) -> Result<()>;
    /// `None` = remove rule.
    fn assign_app(&self, exe: &str, ch: Option<ChannelId>) -> Result<()>;
    fn set_routing_enabled(&self, on: bool) -> Result<()>;
    /// Result lands in `config.mic.voice_level_db`.
    fn start_calibration(&self, seconds: f32) -> Result<()>;
    /// Output EQ of one playback device (endpoint id). `None` removes the entry.
    fn set_output_eq(&self, device_id: &str, eq: Option<OutputEq>) -> Result<()>;
    /// Asks the Clarim service to install Equalizer APO (when missing) and attach it to the physical devices the
    /// channels play on. Returns once the service accepted the job; `snapshot().eq_apo.busy` follows it.
    fn install_eq_apo(&self) -> Result<()>;
    /// Attaches (`enabled`) or detaches Equalizer APO on one playback device (endpoint id). Virtual devices are refused.
    fn set_eq_device(&self, device_id: &str, enabled: bool) -> Result<()>;
    /// Asks the Clarim service to restart Windows Audio (the sound drops for a few seconds); needed after
    /// `eq_apo.needs_restart`. Returns once the service accepted the job.
    fn restart_audio(&self) -> Result<()>;
    fn refresh_devices(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp dir, removed on drop (no dev-dependency on `tempfile`).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("clarim-core-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("config.json")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn json_round_trip() {
        let mut cfg = Config::default();
        cfg.mic.denoise = DenoiseMode::Light;
        cfg.mic.input_id = Some("{0.0.1.00000000}.abc".into());
        cfg.mic.voice_level_db = Some(-23.5);
        cfg.mic.eq = EqPreset::Broadcast;
        cfg.channels.get_mut(&ChannelId::Chat).unwrap().volume = 0.4;
        cfg.channels.get_mut(&ChannelId::Media).unwrap().muted = true;
        cfg.app_rules.insert("game.exe".into(), ChannelId::Aux);
        cfg.default_channel = ChannelId::Media;
        cfg.mic.eq_treble_db = -2.5;
        cfg.output_eq.insert(
            "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}".into(),
            OutputEq { bass_db: 5.0, ..OutputEq::default() },
        );
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"denoise\":\"light\""));
        assert!(json.contains("\"chat\":{"));
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn save_then_load_round_trip_leaves_no_tmp() {
        let dir = TempDir::new("save");
        let mut cfg = Config { port: 51234, ..Config::default() };
        cfg.save_to(&dir.file()).unwrap();
        assert!(!with_suffix(&dir.file(), ".tmp").exists());
        assert_eq!(Config::load_from(&dir.file()), cfg);
        // saving again over an existing file works (rename replaces)
        cfg.routing_enabled = false;
        cfg.save_to(&dir.file()).unwrap();
        assert!(!Config::load_from(&dir.file()).routing_enabled);
    }

    #[test]
    fn partial_json_fills_defaults_and_all_channels() {
        let dir = TempDir::new("partial");
        std::fs::write(
            dir.file(),
            r#"{"mic":{"denoise":"light"},"channels":{"chat":{"volume":0.5}},"app_rules":{"Foo.EXE":"aux"}}"#,
        )
        .unwrap();
        let cfg = Config::load_from(&dir.file());
        assert_eq!(cfg.mic.denoise, DenoiseMode::Light);
        assert!(cfg.mic.enabled && cfg.mic.hpf_on && cfg.mic.auto_level);
        assert_eq!(cfg.mic.voice_focus_db, -14.0);
        assert_eq!(cfg.channels.len(), 4);
        for c in ChannelId::ALL {
            assert!(cfg.channels.contains_key(&c), "missing {c:?}");
        }
        assert_eq!(cfg.channels[&ChannelId::Chat].volume, 0.5);
        assert_eq!(cfg.channels[&ChannelId::Game].volume, 1.0);
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.default_channel, ChannelId::Game);
        assert!(cfg.routing_enabled);
        assert_eq!(cfg.deny_devices, default_deny_devices());
        // hand-edited upper-case rule key is lowercased
        assert_eq!(cfg.channel_for("foo.exe"), ChannelId::Aux);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = TempDir::new("missing");
        assert_eq!(Config::load_from(&dir.file()), Config::default());
    }

    #[test]
    fn corrupt_file_gives_defaults_and_is_renamed() {
        let dir = TempDir::new("corrupt");
        std::fs::write(dir.file(), "{ this is not json").unwrap();
        let cfg = Config::load_from(&dir.file());
        assert_eq!(cfg, Config::default());
        assert!(!dir.file().exists());
        let bad = with_suffix(&dir.file(), ".bad");
        assert_eq!(std::fs::read_to_string(bad).unwrap(), "{ this is not json");
    }

    #[test]
    fn privileged_port_falls_back_to_default() {
        let dir = TempDir::new("port");
        std::fs::write(dir.file(), r#"{"port":80}"#).unwrap();
        assert_eq!(Config::load_from(&dir.file()).port, DEFAULT_PORT);
        std::fs::write(dir.file(), r#"{"port":1024}"#).unwrap();
        assert_eq!(Config::load_from(&dir.file()).port, 1024);
    }

    #[test]
    fn non_utf8_file_is_treated_as_corrupt() {
        let dir = TempDir::new("utf8");
        std::fs::write(dir.file(), [0xff, 0xfe, 0x00, 0x7b]).unwrap();
        assert_eq!(Config::load_from(&dir.file()), Config::default());
        assert!(with_suffix(&dir.file(), ".bad").exists());
    }

    #[test]
    fn call_to_chat_defaults_on_and_survives_missing_key() {
        assert!(Config::default().call_to_chat);
        let cfg: Config = serde_json::from_str(r#"{"port":50000}"#).unwrap();
        assert!(cfg.call_to_chat);
    }

    #[test]
    fn channel_for_is_case_insensitive() {
        let cfg = Config::default();
        assert_eq!(cfg.channel_for("Discord.EXE"), ChannelId::Chat);
        assert_eq!(cfg.channel_for("SPOTIFY.exe"), ChannelId::Media);
        assert_eq!(cfg.channel_for("obs64.exe"), ChannelId::Aux);
        assert_eq!(cfg.channel_for("somegame.exe"), ChannelId::Game);
        let mut cfg = cfg;
        cfg.default_channel = ChannelId::Aux;
        assert_eq!(cfg.channel_for("somegame.exe"), ChannelId::Aux);
    }

    #[test]
    fn default_rules_cover_spec_lists() {
        let r = default_app_rules();
        assert_eq!(r.len(), 15 + 17 + 1);
        assert_eq!(r["ts3client_win64.exe"], ChannelId::Chat);
        assert_eq!(r["whatsapp.root.exe"], ChannelId::Chat);
        assert_eq!(r["vivaldi.exe"], ChannelId::Media);
        assert_eq!(r["potplayermini64.exe"], ChannelId::Media);
        assert!(r.keys().all(|k| *k == k.to_lowercase()));
    }

    #[test]
    fn virtual_device_heuristic() {
        assert!(is_virtual_device("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(is_virtual_device("SteelSeries Sonar - Gaming (SteelSeries Sonar Virtual Audio Device)"));
        assert!(is_virtual_device("Mixagem estéreo (Realtek(R) Audio)"));
        assert!(is_virtual_device("Voicemeeter Input"));
        assert!(!is_virtual_device("Headset Microphone (USB Audio)"));
        assert!(!is_virtual_device("Speakers (Realtek(R) Audio)"));
    }

    const G1: &str = "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}";
    const G2: &str = "{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}";

    fn eq(bass_db: f32, mid_db: f32, treble_db: f32) -> OutputEq {
        OutputEq { bass_db, mid_db, treble_db }
    }

    fn render(entries: &[(&str, OutputEq)]) -> Result<String> {
        render_eqapo_file(&entries.iter().map(|(g, e)| (g.to_string(), *e)).collect::<Vec<_>>())
    }

    #[test]
    fn eqapo_file_exact_output_for_one_device() {
        assert_eq!(
            render(&[(G1, eq(5.0, 0.0, 0.0))]).unwrap(),
            "# Clarim output equalizer - generated, do not edit\n\
             Device: {5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\n\
             Preamp: -5.0 dB\n\
             Filter 1: ON LSC Fc 105 Hz Gain 5.0 dB Q 0.71\n\
             Filter 2: ON PK Fc 1000 Hz Gain 0.0 dB Q 1.0\n\
             Filter 3: ON HSC Fc 8000 Hz Gain 0.0 dB Q 0.71\n\
             Device: all\n"
        );
    }

    #[test]
    fn eqapo_file_preamp_is_the_largest_positive_gain() {
        let preamp = |e| {
            let t = render(&[(G1, e)]).unwrap();
            t.lines().find(|l| l.starts_with("Preamp:")).unwrap().to_string()
        };
        assert_eq!(preamp(eq(-1.0, 2.0, 3.0)), "Preamp: -3.0 dB"); // Clareza
        assert_eq!(preamp(eq(0.0, 0.0, 4.0)), "Preamp: -4.0 dB"); // Agudos
        assert_eq!(preamp(eq(2.25, -6.0, 1.0)), "Preamp: -2.3 dB"); // rounded to 0.1, ties away from zero
        assert_eq!(preamp(eq(-3.0, -1.0, -6.0)), "Preamp: 0.0 dB", "cuts only: no preamp, and never `-0.0`");
        // cuts show their sign on the filter
        let t = render(&[(G1, eq(-1.0, 2.0, 3.0))]).unwrap();
        assert!(t.contains("Filter 1: ON LSC Fc 105 Hz Gain -1.0 dB Q 0.71\n"), "{t}");
        assert!(t.contains("Filter 2: ON PK Fc 1000 Hz Gain 2.0 dB Q 1.0\n"), "{t}");
        assert!(t.contains("Filter 3: ON HSC Fc 8000 Hz Gain 3.0 dB Q 0.71\n"), "{t}");
    }

    #[test]
    fn eqapo_file_clamps_gains_and_refuses_non_finite() {
        let t = render(&[(G1, eq(99.0, -99.0, 12.04))]).unwrap();
        assert!(t.contains("Preamp: -12.0 dB\n") && t.contains("LSC Fc 105 Hz Gain 12.0 dB"), "{t}");
        assert!(t.contains("PK Fc 1000 Hz Gain -12.0 dB") && t.contains("HSC Fc 8000 Hz Gain 12.0 dB"), "{t}");
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(render(&[(G1, eq(bad, 0.0, 0.0))]).is_err());
            assert!(render(&[(G1, eq(0.0, bad, 0.0))]).is_err());
            assert!(render(&[(G1, eq(0.0, 0.0, bad))]).is_err());
        }
        assert_eq!(eq(30.0, -30.0, 0.5).clamped().unwrap(), eq(12.0, -12.0, 0.5));
        assert!(eq(f32::NAN, 0.0, 0.0).clamped().is_err());
    }

    #[test]
    fn eqapo_file_skips_zero_entries_and_always_ends_with_device_all() {
        let none = "# Clarim output equalizer - generated, do not edit\nDevice: all\n";
        assert_eq!(render(&[]).unwrap(), none);
        assert_eq!(render(&[(G1, OutputEq::default())]).unwrap(), none);
        // rounds to 0.0: nothing is written, and no `-0.0` either
        assert_eq!(render(&[(G1, eq(0.04, -0.04, 0.0))]).unwrap(), none);
        let t = render(&[(G1, OutputEq::default()), (G2, eq(0.0, 0.0, 4.0)), (G1, eq(0.0, 3.0, 0.0))]).unwrap();
        assert_eq!(t.matches("Device: ").count(), 3, "two blocks + the final reset: {t}");
        assert!(t.contains(&format!("Device: {G2}\nPreamp: -4.0 dB\n")), "{t}");
        assert!(t.find(&format!("Device: {G2}")).unwrap() < t.find(&format!("Device: {G1}")).unwrap(), "input order kept");
        assert!(t.ends_with("Device: all\n") && t.matches("Device: all").count() == 1);
        assert!(!t.contains('\r') && !t.contains("-0.0"));
    }

    #[test]
    fn eqapo_file_rejects_anything_but_a_braced_guid() {
        for bad in [
            "",
            "{}",
            "5D16E8DA-37A3-4657-8D52-57FFE97F4EF9",     // no braces
            "{5D16E8DA37A3-4657-8D52-57FFE97F4EF9}",    // hyphen missing
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF}",    // one digit short
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9A}",  // one digit long
            "{5D16E8DG-37A3-4657-8D52-57FFE97F4EF9}",   // not hex
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\n", // trailing newline
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9} ",  // trailing space
            " {5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}",  // leading space
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\nInclude: C:\\x.txt",
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\r\nVSTPlugin: Library x.dll",
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}; all", // second Device pattern
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4E\u{661}9}", // non-ASCII digit
            "all",
        ] {
            assert!(render(&[(bad, eq(1.0, 0.0, 0.0))]).is_err(), "{bad:?}");
            // checked even when the entry would be skipped as all-zero
            assert!(render(&[(bad, OutputEq::default())]).is_err(), "{bad:?}");
        }
        // one bad entry spoils the whole file: nothing half-written
        assert!(render(&[(G1, eq(1.0, 0.0, 0.0)), ("{x}", eq(1.0, 0.0, 0.0))]).is_err());
        assert!(render(&[(G1, eq(1.0, 0.0, 0.0))]).is_ok());
        assert!(render(&[(G2, eq(1.0, 0.0, 0.0))]).is_ok(), "lower-case hex is fine");
    }

    #[test]
    fn endpoint_guid_takes_the_part_after_the_first_brace_dot() {
        assert_eq!(endpoint_guid("{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}"), Some(G1));
        assert_eq!(endpoint_guid("{0.0.1.00000000}.{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}"), Some(G2));
        for bad in [
            "",
            G1, // already a bare GUID: no "}." prefix
            "{0.0.0.00000000}",
            "{0.0.0.00000000}.",
            "{0.0.0.00000000}.abc",
            "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}x",
            "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\n",
            "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}",
            "out-rtk",
        ] {
            assert_eq!(endpoint_guid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn new_fields_default_when_missing_from_old_json() {
        // a config.json written before the equalizers existed
        let cfg: Config = serde_json::from_str(r#"{"mic":{"eq":"warm"},"port":50000}"#).unwrap();
        assert_eq!(cfg.mic.eq, EqPreset::Warm);
        assert_eq!((cfg.mic.eq_bass_db, cfg.mic.eq_mid_db, cfg.mic.eq_treble_db), (0.0, 0.0, 0.0));
        assert!(cfg.output_eq.is_empty());
        let cfg: Config = serde_json::from_str(r#"{"output_eq":{"id":{"bass_db":5}}}"#).unwrap();
        assert_eq!(cfg.output_eq["id"], eq(5.0, 0.0, 0.0), "missing gains are 0");
        // and an old dashboard state without `eq_apo`
        let mut snap = serde_json::to_value(Snapshot {
            version: "t".into(),
            config: Config::default(),
            inputs: vec![],
            outputs: vec![],
            sessions: vec![],
            mic: MicStatus::default(),
            virtual_mic_found: false,
            eq_apo: EqApoStatus::default(),
        })
        .unwrap();
        assert_eq!(snap["eq_apo"]["writer"], "none");
        assert_eq!(snap["eq_apo"]["installed"], false);
        snap.as_object_mut().unwrap().remove("eq_apo");
        let back: Snapshot = serde_json::from_value(snap).unwrap();
        assert_eq!(back.eq_apo, EqApoStatus::default());
    }

    #[test]
    fn eq_apo_status_from_an_older_agent_gets_the_new_fields_by_default() {
        // the status as it was before busy / needs_restart / apo_loaded / devices existed
        let old = r#"{"installed":true,"writer":"service","active_devices":["a"],"unknown_devices":[],
                      "enhancements_off":[],"error":null}"#;
        let st: EqApoStatus = serde_json::from_str(old).unwrap();
        assert!(st.installed && st.writer == "service" && st.active_devices == ["a"]);
        assert_eq!((st.busy, st.needs_restart, st.apo_loaded, st.devices.len()), (None, false, None, 0));
        let st = EqApoStatus {
            busy: Some("Instalando...".into()),
            needs_restart: true,
            apo_loaded: Some(false),
            devices: vec![EqApoDevice { guid: G1.into(), configured: true, can_configure: true, enhancements_off: false }],
            ..EqApoStatus::default()
        };
        let back: EqApoStatus = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn braced_guid_check_is_public_and_strict() {
        assert!(is_braced_guid(G1) && is_braced_guid(G2));
        assert!(!is_braced_guid("") && !is_braced_guid("{x}") && !is_braced_guid(&format!("{G1}\n")));
    }

    #[test]
    fn apo_stock_config_is_recognised_however_it_is_spelled_and_nothing_else_is() {
        let stock = APO_STOCK_CONFIG.join("\r\n"); // the real file: CRLF, no final newline
        assert_eq!(stock.len(), 157, "the file APO 1.4.2 ships");
        assert!(is_stock_apo_config(&stock));
        assert!(is_stock_apo_config(&APO_STOCK_CONFIG.join("\n")), "LF");
        assert!(is_stock_apo_config(&format!("{}\r\n", stock)), "final newline");
        assert!(is_stock_apo_config(&format!("\u{feff}{stock}")), "UTF-8 BOM");
        let spaced = APO_STOCK_CONFIG.map(|l| format!("  {}  \t", l.replace(' ', "   "))).join("\r\n\r\n");
        assert!(is_stock_apo_config(&spaced), "trailing spaces, wide gaps and blank lines");
        assert!(is_stock_apo_config(&stock.to_uppercase()), "case does not matter");
        assert!(is_stock_apo_config(&format!("{stock}\nInclude: clarim-eq.txt\n")), "with Clarim's include appended");
        assert!(is_stock_apo_config(&format!("{stock}\r\n\r\n  include: CLARIM-EQ.TXT \r\n")));
        // the user changed something: not ours to replace
        assert!(!is_stock_apo_config(&stock.replace("-6 dB", "-5 dB")));
        assert!(!is_stock_apo_config(&format!("{stock}\nChannel: all\n")), "an extra user line");
        assert!(!is_stock_apo_config(&format!("{stock}\nInclude: clarim-eq.txt\nChannel: all\n")));
        assert!(!is_stock_apo_config(&format!("Include: clarim-eq.txt\n{stock}\n")), "the include goes last");
        assert!(!is_stock_apo_config(&format!("{stock}\nInclude: clarim-eq.txt\nInclude: clarim-eq.txt\n")));
        assert!(!is_stock_apo_config(&format!("# my notes\n{stock}")));
        assert!(!is_stock_apo_config(&APO_STOCK_CONFIG[..2].join("\n")), "a line removed");
        assert!(!is_stock_apo_config(&stock.replace("GraphicEQ: 25 0", "GraphicEQ: 25 1")));
        assert!(!is_stock_apo_config("") && !is_stock_apo_config("Include: clarim-eq.txt\n"));
    }

    #[test]
    fn channel_labels() {
        assert_eq!(ChannelId::Media.label(), "Mídia");
        assert_eq!(ChannelId::ALL.map(ChannelId::label), ["Jogo", "Chat", "Mídia", "Aux"]);
    }
}
