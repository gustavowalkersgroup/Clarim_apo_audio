# Clarim — build spec (for coding agents)

Lightweight, open-source Windows 10/11 audio tool. Two jobs, nothing else:

1. **Mic optimizer**: real mic → AI noise suppression + background-voice rejection + voice AGC (speech held near −23 dBFS) → virtual mic (VB-Cable "CABLE Input" etc.). Discord/Meet/Zoom pick the virtual mic's capture side.
2. **Output channels** Game / Chat / Media / Aux: every app that plays audio belongs to one channel; each channel has an output device, a volume and a mute. Spotify/YouTube → Media, Discord/Meet → Chat, games → Game.

Runs headless as a per-user **agent** kept alive by a **Windows service**; controlled by a small **local web dashboard** (`http://127.0.0.1:47800`) opened as an Edge app window from a tray icon.

Reference implementation to reuse (read it, copy what fits): `F:\bkp\Documents\Claude\Claudin\Modulador de vos\crates\vox-engine` (WASAPI capture/render pipeline, device enumeration, RNNoise, biquad EQ, gain/gate/compressor/limiter). It builds on this machine (Rust 1.96 stable MSVC, wasapi 0.15, windows 0.58, nnnoiseless 0.5, ringbuf 0.4). Its `Cargo.lock` pins versions that compile — copy it into the new workspace as the starting lock file.

Research notes (exact COM GUIDs, crate APIs, gotchas): `docs/research.md`. Read the section for your crate before coding.

## Design decisions (do not relitigate)

- **Channels need no virtual driver.** A channel is a group of apps. Routing = Windows per-app default endpoint (undocumented `IAudioPolicyConfigFactory`, same mechanism EarTrumpet uses) pointing each app's process at its channel's real output device. Channel volume/mute = `ISimpleAudioVolume` on each of that channel's app sessions. Zero DSP, zero added latency for outputs. Known limit: per-process, so a browser tab can't be split from another tab of the same browser.
- **Mic needs one virtual cable** (VB-Cable free, or an already-installed one). Auto-detect order for the render target: `CABLE Input` (VB-Cable) → any render endpoint whose name contains `Virtual Audio Driver` / `Virtual Mic` → `SteelSeries Sonar - Microphone`. Never hard-fail when none exists: report `no virtual mic found — install VB-Cable` in status.
- **Denoise modes**: `off`, `light` = RNNoise (nnnoiseless), `strong` = DeepFilterNet3 (Rust crate, tract backend, embedded model). If DFN fails to init at runtime, fall back to RNNoise and report it.
- **Background voices** ("conversas paralelas"): no open model is light enough for target-speaker extraction, so: *Voice Focus* = downward expander keyed on post-denoise RMS, threshold = calibrated user voice level + `voice_focus_db` (negative), AND gated by speech probability (RNNoise VAD or DFN lsnr). User calibrates by speaking 5 s. Near-field voice is typically 15–25 dB louder at the mic than people across the room — that is what this exploits. Be honest about it in UI text.
- Internal mic format: 48 kHz mono f32, 480-sample frames (10 ms). Must not allocate or lock in the per-frame DSP path.
- One exe, `clarim.exe`, subcommands. Single process agent. No Electron, no webview bundle, no Node build step: the dashboard is one hand-written `dashboard.html` (vanilla JS + CSS) embedded with `include_str!`.
- UI language: Brazilian Portuguese. Code, comments, logs: English.
- Prefer few dependencies. Allowed new deps: `tiny_http = "0.12"`, `windows-service = "0.8"`, `tray-icon = "0.26"` (+ its `muda` re-export), DeepFilterNet, `ndarray = "0.15"` (must match libDF), `log`, `env_logger` (or a file logger writing to `%LOCALAPPDATA%\Clarim\clarim.log`). Ask (via your report) before adding anything else. Stay on `windows = "0.58"`, `wasapi = "0.15"`, `ringbuf = "0.4"` (cached, known to build; upgrading is out of scope).
- **DeepFilterNet dependency**: the crates.io `deep_filter` (0.2.5, 2022) is stale — use git: `deep_filter = { git = "https://github.com/Rikorose/DeepFilterNet", rev = "<pin a commit>", default-features = false, features = ["tract", "default-model"] }` (lib name `df`; check whether the import path is `df::tract::{DfParams, DfTract, RuntimeParams}`). libDF pins ndarray 0.15 and tract 0.21. Put it behind a cargo feature `dfn` (default on) in clarim-audio so the app still builds without it.

## Workspace layout

```
Clarim/
  Cargo.toml            # workspace; members below; release profile opt-level=3, lto="thin", codegen-units=1; dev deps opt-level=2
  Cargo.lock            # start from Modulador's
  SPEC.md  README.md  docs/research.md
  crates/
    clarim-core/        # shared types, config persistence, Backend trait. Deps: serde, serde_json, anyhow, dirs, log. NO windows deps.
    clarim-audio/       # mic engine. Deps: clarim-core, wasapi, nnnoiseless, deep_filter(+ndarray), ringbuf, crossbeam-channel, windows
    clarim-routing/     # per-app sessions + policy routing. Deps: clarim-core, windows
    clarim-web/         # HTTP server + dashboard.html. Deps: clarim-core, tiny_http, serde_json
    clarim-svc/         # Windows service supervisor + install/uninstall + output-EQ pipe server (Equalizer APO writer). Deps: clarim-core, serde, serde_json, windows-service, windows
  apps/clarim/          # bin `clarim`: CLI, Backend impl wiring audio+routing+config, tray icon, single instance
```

Each library crate must build and test on its own: `cargo build -p <crate>` / `cargo test -p <crate>`. Library crates depend only on `clarim-core` among workspace crates.

## clarim-core (the contract — everyone codes against this)

```rust
pub const DEFAULT_PORT: u16 = 47800;
pub const APP_NAME: &str = "Clarim";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ChannelId { Game, Chat, Media, Aux }
impl ChannelId {
    pub const ALL: [ChannelId; 4] = [ChannelId::Game, ChannelId::Chat, ChannelId::Media, ChannelId::Aux];
    pub fn label(self) -> &'static str; // "Jogo", "Chat", "Mídia", "Aux"
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum DenoiseMode { Off, Light, Strong }

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum EqPreset { Flat, Clarity, Warm, Broadcast }

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct MicSettings {
    pub enabled: bool,               // true
    pub input_id: Option<String>,    // None = auto-pick real mic
    pub output_id: Option<String>,   // None = auto-pick virtual cable render endpoint
    pub monitor_id: Option<String>,  // headphones for self-listening
    pub monitor_on: bool,            // false
    pub denoise: DenoiseMode,        // Strong
    pub denoise_strength: f32,       // 0..1, default 1.0. RNNoise: wet mix. DFN: attenuation limit (0 → 6 dB … 1 → 100 dB, log-ish)
    pub voice_focus: bool,           // false until calibrated? default false
    pub voice_focus_db: f32,         // -40..0, default -14 (threshold relative to calibrated voice level)
    pub voice_level_db: Option<f32>, // calibrated speech RMS in dBFS; None = not calibrated (use -26 dBFS assumption)
    pub gate_on: bool,               // false
    pub gate_threshold_db: f32,      // -50
    pub hpf_on: bool,                // true  (80 Hz high-pass, removes rumble/desk bumps)
    pub auto_level: bool,            // true  (voice AGC: keeps the speech level near -23 dBFS (dB mean of 10 ms frame RMS; gain -12..+20 dB), learns only from the user's speech, does not raise the noise between words above -55 dBFS; the -1 dBFS limiter stays after it; see "Auto level")
    pub eq: EqPreset,                // Flat
    pub eq_bass_db: f32,             // 0  (-12..+12) tone sliders, applied after the `eq` preset: low shelf / mid peak / high shelf (corner frequencies: see clarim-audio)
    pub eq_mid_db: f32,              // 0  (-12..+12)
    pub eq_treble_db: f32,           // 0  (-12..+12)
    pub input_gain_db: f32,          // 0  (-20..+20)
    pub output_gain_db: f32,         // 0  (-20..+20)
    pub leak_warnings: bool,         // true  (during calls, warn (tray balloon without sound + dashboard banner) when sound still leaks into the virtual mic; see "Call tick")
    pub auto_mic_volume: bool,       // true  (during calls, with auto_level on, lower the real mic's Windows volume in 3 dB steps when the input clips on speech: at most 12 dB in total, only mics with a hardware volume control; give back what Clarim took when the voice stays too quiet, and on quit)
    pub mic_volume_taken: Option<MicVolumeTaken>, // None  (Clarim's bookkeeping of that, never edited by the dashboard: `check_mic` keeps the stored value, exactly like `voice_level_db`)
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct MicVolumeTaken { pub device_id: String, pub baseline_db: f32, pub taken_db: f32 } // endpoint id, the volume before Clarim lowered it, the dB taken so far (0..12)

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum LeakKind { Echo, Noise }

/// One app capture session on a capture endpoint (from the routing worker).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CaptureSession { pub pid: u32, pub exe: String, pub device_id: String, pub active: bool }

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ChannelSettings {
    pub device_id: Option<String>,   // None = Windows default render device
    pub volume: f32,                 // 0..1, default 1.0
    pub muted: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Config {
    pub version: u32,                                  // 1
    pub mic: MicSettings,
    pub channels: BTreeMap<ChannelId, ChannelSettings>, // always all 4 keys after load()
    pub app_rules: BTreeMap<String, ChannelId>,        // key = lowercase exe file name, e.g. "spotify.exe"
    pub default_channel: ChannelId,                    // Game — channel for apps without a rule
    pub routing_enabled: bool,                         // true — false = observe only, never touch app routing/volume
    pub deny_devices: Vec<String>,                     // case-insensitive substrings hidden from device lists
    pub port: u16,                                     // DEFAULT_PORT (a loaded value < 1024 is reset to DEFAULT_PORT)
    pub call_to_chat: bool,                            // true — Media apps that start using a mic (browser calls: Meet, Teams web, Discord web) play on Chat until 20 s after the call
    pub output_eq: BTreeMap<String, OutputEq>,         // {} — output equalizer per playback DEVICE (key = endpoint id); a missing key = no EQ; see "Output equalizer (Equalizer APO)"
}
impl Config {
    pub fn path() -> PathBuf;              // %APPDATA%\Clarim\config.json (dirs::config_dir)
    pub fn load() -> Config;               // missing/corrupt/non-UTF-8 file → defaults (corrupt file renamed to config.json.bad); fills missing channels
    pub fn save(&self) -> anyhow::Result<()>; // atomic: write config.json.tmp, sync_all, then rename
    pub fn channel_for(&self, exe: &str) -> ChannelId; // rule lookup, case-insensitive, else default_channel
}
pub fn default_app_rules() -> BTreeMap<String, ChannelId>;
// Chat:  discord.exe, teams.exe, ms-teams.exe, zoom.exe, slack.exe, whatsapp.exe, telegram.exe, skype.exe, signal.exe, mumble.exe, teamspeak.exe, ts3client_win64.exe, discordptb.exe, discordcanary.exe, whatsapp.root.exe
// Media: spotify.exe, chrome.exe, msedge.exe, firefox.exe, brave.exe, opera.exe, vivaldi.exe, vlc.exe, foobar2000.exe, musicbee.exe, aimp.exe, itunes.exe, applemusic.exe, deezer.exe, tidal.exe, mpc-hc64.exe, potplayermini64.exe
// Aux:   obs64.exe
// (games fall to default_channel = Game)
pub fn default_deny_devices() -> Vec<String>; // ["NVIDIA High Definition Audio", "NVIDIA Broadcast"]

/// Heuristic: endpoint is a virtual/loopback device, not real hardware.
pub fn is_virtual_device(name: &str) -> bool; // "cable", "sonar", "virtual", "steam streaming", "nvidia broadcast", "voicemeeter", "stereo mix", "mixagem estéreo"

pub const EQ_TONE_RANGE_DB: f32 = 12.0;  // every tone slider (mic and output): -12..+12 dB

/// Output tone controls of one playback device (low shelf / mid peak / high shelf, dB).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct OutputEq { pub bass_db: f32, pub mid_db: f32, pub treble_db: f32 }
impl OutputEq { pub fn clamped(self) -> anyhow::Result<OutputEq>; } // clamp to ±12 dB; NaN/inf → PT-BR error

/// Equalizer APO state for the output EQ, filled by the agent (from the Clarim service's status when it answers, else from what a
/// normal user can read). Every field after `error` is #[serde(default)]. `Default`: installed false, writer "none", empty lists, no error.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct EqApoStatus {
    pub installed: bool,               // <Program Files>\EqualizerAPO\EqualizerAPO.dll exists; with the service answering, a COMPLETE install (DLL + HKLM\SOFTWARE\EqualizerAPO InstallPath + both classes registered): a DLL left by an interrupted install is "not installed" and shows the install button again
    pub writer: String,                // "service" (pipe to the Clarim service: it can also install, attach and restart) | "direct" (config dir writable by the user: config only) | "none"
    pub active_devices: Vec<String>,   // render endpoint ids Equalizer APO is attached to (one of the effect slots holds an APO CLSID)
    pub unknown_devices: Vec<String>,  // endpoint ids whose state is not known (not in the service's list, or the registry could not be read)
    pub enhancements_off: Vec<String>, // endpoint ids with Windows "audio enhancements" disabled: APO does not run there
    pub error: Option<String>,         // PT-BR: the service's last install/restart failure or warning, else the last config-write failure, else why nobody can write
    pub busy: Option<String>,          // PT-BR progress text while the service installs APO or restarts Windows Audio ("Baixando...", "Instalando...", "Configurando os aparelhos...", "Reiniciando o áudio do Windows..."); None otherwise
    pub needs_restart: bool,           // an attach/detach (or an install) is newer than the running audio graph (audiodg.exe): no effect until Windows Audio restarts
    pub apo_loaded: Option<bool>,      // Some(true/false) = a running audiodg.exe has / has not loaded EqualizerAPO.dll; None = cannot tell. Information only (see "Equalizer APO management")
    pub devices: Vec<EqApoDevice>,     // the service's view of every present playback endpoint (empty without the service)
}

/// One playback endpoint as the service sees it (registry only).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct EqApoDevice {
    pub guid: String,                  // "{8-4-4-4-12}" as named in the registry (endpoint_guid of the endpoint id; compare case-insensitively)
    pub configured: bool,              // an effect slot holds an Equalizer APO class id (also installs made by DeviceSelector)
    pub can_configure: bool,           // present and not a virtual device (cable, Sonar, Voicemeeter...)
    pub enhancements_off: bool,        // Disable_SysFx = 1 in FxProperties or in Properties
}

/// "{0.0.0.00000000}.{GUID}" -> "{GUID}": the part after the first "}.". None unless that part is a well-formed
/// braced GUID, so a Some is safe for `render_eqapo_file` and for a registry path.
pub fn endpoint_guid(endpoint_id: &str) -> Option<&str>;
/// `^\{[0-9A-Fa-f]{8}-([0-9A-Fa-f]{4}-){3}[0-9A-Fa-f]{12}\}$`, no trailing newline allowed. Used by the service for every GUID that reaches a registry path.
pub fn is_braced_guid(s: &str) -> bool;
/// The whole `clarim-eq.txt`; the ONLY producer of that file. See "Output equalizer (Equalizer APO)".
pub fn render_eqapo_file(entries: &[(String /* endpoint GUID "{8-4-4-4-12}" */, OutputEq)]) -> anyhow::Result<String>;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceInfo { pub id: String, pub name: String, pub is_default: bool, pub is_virtual: bool }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppSession {
    pub pid: u32,
    pub exe: String,            // lowercase file name
    pub name: String,           // display name (session display name, else exe without .exe)
    pub channel: ChannelId,     // EFFECTIVE channel: Config::channel_for, except Chat while in_call
    pub has_rule: bool,         // exe present in app_rules
    pub in_call: bool,          // #[serde(default)] — true while this Media app captures a microphone and is therefore played on Chat
    pub device_id: String,      // endpoint the session currently lives on
    pub active: bool,           // AudioSessionStateActive
    pub peak: f32,              // 0..1
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MicStatus {
    pub running: bool,
    pub error: Option<String>,        // human-readable PT-BR when possible
    pub input_name: Option<String>,
    pub output_name: Option<String>,  // virtual mic actually in use
    pub denoise_active: Option<DenoiseMode>, // what is really running (Strong may fall back to Light)
    pub in_peak: f32, pub out_peak: f32, // 0..1
    pub vad: f32,                     // 0..1 speech probability
    pub focus_open: bool,             // voice focus currently passing audio
    pub dsp_us: u32, pub latency_ms: f32,
    pub calibrating: bool,
    pub calibration_progress: f32,    // 0..1
    // Every field below is #[serde(default)]. Engine fields, filled by MicEngine::status() (see "Auto level" and "Leak detection"):
    pub agc_gain_db: f32,             // current AGC gain in dB, 0 when auto_level is off
    pub input_voice_db: Option<f32>,  // AGC input speech level, dBFS; None until confident
    pub voice_db: Option<f32>,        // speech level at the cable (input_voice_db + AGC gain + output gain), dBFS; None until confident
    pub echo_score: f32,              // 0..1: max over the running loopback references that have an active reference (0 otherwise)
    pub echo_db: Option<f32>,         // leak level of the argmax detector, dBFS
    pub echo_lag_ms: f32,             // best lag of the argmax detector
    pub noise_db: Option<f32>,        // residual noise at the output during non-speech frames, dBFS
    pub clip_events: u32,             // monotonic count of clipped raw input speech frames since the engine started; resets on a restart (consumers use saturating_sub and treat a decrease as a reset)
    pub references: u32,              // loopback reference streams running
    // Backend overlay: the engine leaves these at their defaults, the agent fills them in `mic_status()` and `snapshot()`:
    pub in_call: bool,                // a non-ignored app has an Active capture session on the virtual mic's capture endpoint (8 s hangover)
    pub call_apps: Vec<String>,       // exe names of those apps, deduped and sorted, unknown left out
    pub leak: Option<LeakKind>,       // the current leak alert, only while in_call
    pub mic_volume_note: Option<String>, // PT-BR note about the last Windows mic volume action: shown 60 s, except the "saturating before the Windows volume" note, which stays for the run
    pub mic_too_quiet: bool,          // the voice stays low with the AGC at its maximum and no Windows volume left to give back
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meters { pub mic: MicStatus, pub sessions: Vec<(u32, f32)> } // (pid, peak)

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub version: String,
    pub config: Config,
    pub inputs: Vec<DeviceInfo>,   // deny-list filtered
    pub outputs: Vec<DeviceInfo>,  // deny-list filtered (virtual ones included, flagged)
    pub sessions: Vec<AppSession>,
    pub mic: MicStatus,
    pub virtual_mic_found: bool,
    pub eq_apo: EqApoStatus,       // #[serde(default)]
}

/// Implemented by apps/clarim; consumed by clarim-web. All methods are cheap and non-blocking
/// (heavy work happens on engine threads). Errors are user-facing strings (PT-BR ok).
pub trait Backend: Send + Sync + 'static {
    fn snapshot(&self) -> Snapshot;
    fn meters(&self) -> Meters;
    fn set_mic(&self, s: MicSettings) -> anyhow::Result<()>;          // persists + applies live
    fn set_channel(&self, id: ChannelId, s: ChannelSettings) -> anyhow::Result<()>;
    fn assign_app(&self, exe: &str, ch: Option<ChannelId>) -> anyhow::Result<()>; // None = remove rule
    fn set_routing_enabled(&self, on: bool) -> anyhow::Result<()>;
    fn start_calibration(&self, seconds: f32) -> anyhow::Result<()>;   // result lands in config.mic.voice_level_db
    fn set_output_eq(&self, device_id: &str, eq: Option<OutputEq>) -> anyhow::Result<()>; // None = remove; persists config.output_eq and (app) writes the Equalizer APO file
    fn install_eq_apo(&self) -> anyhow::Result<()>;                    // service installs Equalizer APO if missing and attaches it to the physical devices the 4 channels use; returns when the service accepted the job (eq_apo.busy follows it)
    fn set_eq_device(&self, device_id: &str, enabled: bool) -> anyhow::Result<()>; // attach / detach APO on one playback device (endpoint id); virtual devices refused
    fn restart_audio(&self) -> anyhow::Result<()>;                     // service restarts Windows Audio; returns when accepted
    fn refresh_devices(&self);
}
```

Defaults on first run (in apps/clarim, not core; first run = no `config.json` yet AND a device scan that worked: after a failed scan nothing is saved, so the next start retries): if the Windows default render device is virtual (e.g. "CABLE Input", or the legacy "SteelSeries Sonar - Gaming"), set every channel's `device_id` to the first non-virtual, non-denied render device (analog ones before S/PDIF/HDMI/digital ones, which usually have nothing plugged in); otherwise leave `None`. Also on first run: if any render device name contains `sonar` (case-insensitive), start with `routing_enabled = false`, so a first launch never fights SteelSeries Sonar over every app's output; the dashboard says so and the user turns routing on.

### Output equalizer (Equalizer APO)

Clarim's channels never touch app audio (per-app device routing only), so the output EQ is Equalizer APO configuration, one EQ per playback DEVICE (not per channel). The dashboard offers presets Neutro 0/0/0, Mais graves +5/0/0, Clareza -1/+2/+3, Agudos 0/0/+4 dB (bass/mid/treble) plus three sliders Graves/Médios/Agudos (-12..+12 dB); the contract is only the three gains. The mic EQ (preset + the same three sliders after it) is the separate `MicSettings` pair. Research and the unverified facts: `docs/eqapo.md`.

- Config dir `<FOLDERID_ProgramFiles>\EqualizerAPO\config` (`config.txt`); installed = `EqualizerAPO.dll` exists in `<ProgramFiles>\EqualizerAPO`. Clarim owns ONE file there, `clarim-eq.txt`, and makes sure `config.txt` has the line `Include: clarim-eq.txt` (appended once with a leading newline; the original is backed up once as `config.txt.clarim-backup`; no other line is touched, with one exception: when `config.txt` is exactly Equalizer APO's stock default, `clarim_core::is_stock_apo_config` (the 157-byte file APO 1.4.2 ships: `Preamp: -6 dB`, `Include: example.txt`, a flat `GraphicEQ`; compared after BOM strip, trim, whitespace collapse, case and empty lines; with or without Clarim's own include line appended), it is replaced by just the include line, because APO applies `config.txt` to every attached device and the stock one lowers the volume by 6 dB and adds a bass boost; the backup keeps the stock file. This runs in every writer (service `ensure_include`, agent direct path) and, best effort, as soon as the service attaches APO to a device or finishes installing it; a failure there is only a warning). APO reloads the files in its config dir live.
- `render_eqapo_file` output: the comment line `# Clarim output equalizer - generated, do not edit`, then per device with a non-zero EQ `Device: {GUID}`, `Preamp: -X.X dB`, `Filter 1: ON LSC Fc 105 Hz Gain G dB Q 0.71`, `Filter 2: ON PK Fc 1000 Hz Gain G dB Q 1.0`, `Filter 3: ON HSC Fc 8000 Hz Gain G dB Q 0.71`, and a final `Device: all` (so nothing after our `Include:` is scoped to the last device). X = the largest positive gain, 0.0 when there is none (written `Preamp: 0.0 dB`, never `-0.0`). Gains are clamped to ±12 dB (NaN/inf = error) and rounded to 0.1 dB, always `.` decimal; an entry whose gains all round to zero is skipped; the file ends with `\n`, lines are `\n`-separated. A GUID that is not exactly `{8-4-4-4-12}` hex digits (no spaces, no newline) is an error for the whole file, so no client text can reach the file and add a `VSTPlugin:`/`Include:`/`Convolution:` line (the Windows audio engine loads it). The `Device:` pattern (a braced GUID, unverified: APO matches name, connection name and GUID) lives in one helper, `device_line`.
- Writer A (preferred, Clarim service installed): the LocalSystem service serves the named pipe `\\.\pipe\ClarimEqApo` (SDDL `D:(A;;0x120183;;;IU)(A;;GA;;;SY)(A;;GA;;;BA)`: interactive users get read/write data and attributes, read control and synchronize, which is all a client needs, and NOT `FILE_CREATE_PIPE_INSTANCE`, so no user can add an instance of the pipe and answer the agent in the service's place; the client opens with the matching explicit rights, never `GENERIC_WRITE`; byte mode, one request per connection). The original request is one JSON line of at most 8 KiB `{"entries":[{"guid":"{...}","bass_db":0,"mid_db":0,"treble_db":0}]}` (a line without `cmd`); reply = one JSON line `{"ok":true}` or `{"ok":false,"error":"..."}`; read/connect timeouts. The pipe also carries the management commands of "Equalizer APO management" (a request with a `cmd` key; replies up to 64 KiB). The service resolves the config dir itself (`SHGetKnownFolderPath`), never from the request, renders with `render_eqapo_file`, writes atomically (tmp + rename) and ensures the Include line. That folder is writable by every user (Equalizer APO's installer grants `BUILTIN\Users` full control on `config` and `config.txt`, checked with `icacls` on the development PC), and the service works in it as LocalSystem, so it never follows a link: every file there is opened with `FILE_FLAG_OPEN_REPARSE_POINT` (the link itself, not its target), `config.txt` is read only when it is a plain file of at most 1 MiB (otherwise the request fails with a PT-BR error, nothing is changed), the `config.txt.clarim-backup` copy is written from the bytes already read with `create_new` (a name that already exists, a planted link included, is left alone), and the `clarim-eq.txt.tmp` file is deleted (a link is removed, not its target) and created new. `config.txt` is opened for writing only when the Include line has to be added or removed. Any account can still edit `config.txt` (for example add a `VSTPlugin:` line): that is Equalizer APO's own design, and the README says so. Writer B: when the config dir is writable by the current user (probe: create and delete a temp file) the agent writes directly with the same renderer (config only: installing, attaching and restarting always need the service). Otherwise `EqApoStatus.writer == "none"` and the dashboard says the service must be installed.
- Detection (agent): with the service answering (`writer == "service"`), `eq_apo` is the service's status (`status` command, read as SYSTEM, which also sees installs made by DeviceSelector and checks both places of the enhancements flag): `installed`, `busy`, `error`, `needs_restart`, `apo_loaded`, `devices` copied, and `active_devices` / `enhancements_off` / `unknown_devices` mapped from `devices` to the agent's endpoint ids by GUID (case-insensitive; a device the service does not list is unknown, so the dashboard claims nothing). A service that answers writes but not `status` (an older one) leaves `writer == "service"` and an `error` that tells the user to run `clarim.exe install` again. Without the service (or under `--dry-run`, which never calls it) the agent reads what a normal user can, read-only, hints only: APO is active on a render endpoint when any value under `HKLM\...\MMDevices\Audio\Render\{GUID}\FxProperties` contains (case-insensitive) either Equalizer APO class id (`{EACD2258-FCAC-4FF4-B36D-419E924A6D79}` pre-mix, `{EC1CC9CE-FAED-4822-828A-82A81A6F018F}` post-mix); unreadable = `unknown_devices`; enhancements are off when `{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5` is DWORD 1 under the endpoint's `Properties` or `FxProperties`.
- Verified on the development PC (Equalizer APO 1.4.2 installed with DeviceSelector, read-only): the registry layout the planner models ("Equalizer APO management", ground truth in `docs/eqapo.md`). Still NOT verified live, to check on first use: that Clarim's own writes (attach, detach, install, restart) leave a working audio engine (all of it is unit-tested against fakes and the DeviceSelector dumps, none of it was run against the real registry, installer or Audiosrv); the braced-GUID `Device:` match (lower-case GUIDs: that is how Windows reports endpoint ids and names the `MMDevices` keys here); that APO resolves the relative `Include: clarim-eq.txt` against the folder of `config.txt`; that a live-reload fires on the tmp + rename write; that a medium-integrity agent can open the pipe the LocalSystem service creates (the SDDL has no explicit integrity label). A wrong `Device:` pattern fails silently (the EQ just does nothing), which is why the dashboard shows the per-device status.

### Equalizer APO management (install, attach, restart)

The user never opens Equalizer APO's own tools (its installer, `DeviceSelector.exe`, `Editor.exe`): the Clarim service, as LocalSystem, installs Equalizer APO, attaches it to playback devices and restarts Windows Audio on request of the dashboard. Everything the service does for these commands is validated on the service side (the pipe is a trust boundary: any interactive user can connect), and a request can only name endpoint GUIDs that exist, never a path, URL or command.

Pipe commands (one request per connection, one JSON line each way; request <= 8 KiB, reply <= 64 KiB; the server never logs request content; garbage or an unknown `cmd` = `{"ok":false,"error":"Pedido inválido."}`):

| request line | service does | reply |
|---|---|---|
| `{"entries":[...]}` | writes `clarim-eq.txt` (unchanged, see Writer A) | `{"ok":true}` / `{"ok":false,"error":...}` |
| `{"cmd":"status"}` | nothing | `{"ok":true,"status":{installed, busy, error, needs_restart, apo_loaded, devices:[{guid, configured, can_configure, enhancements_off}]}}` |
| `{"cmd":"install_apo","device_guids":["{..}"]}` | installs APO if missing, then attaches each device; on a worker thread | `{"ok":true}` once accepted. Refused: more than 16 GUIDs or a malformed one ("Pedido inválido."), another operation running ("Já há uma operação em andamento: <busy text>") |
| `{"cmd":"set_device","guid":"{..}","enabled":true}` | attach / detach on one endpoint, inline (milliseconds) | `{"ok":true}` / error. Attach needs APO installed and a present, non-virtual device; detach is always allowed |
| `{"cmd":"restart_audio"}` | restarts Windows Audio; on a worker thread | `{"ok":true}` once accepted. Refused within 10 s of the previous restart |

One operation at a time (install, restart and `set_device` share one slot; the busy text names the running one). `status` is cheap (registry reads, one process-module look) and the agent polls it every 10 s, every housekeeping tick (1 s) while `busy` is set, and right after each command. `busy`/`error` rules: `error` is the PT-BR outcome of the LAST install or restart (cleared when the next operation starts; a warning such as "O áudio voltou, mas estes serviços não: ..." counts) and is not set by `set_device` (its error is in the reply).

`status.devices` lists every endpoint under `MMDevices\Audio\Render` whose `DeviceState` does not have the "not present" bit (4), at most 100: `configured` = any of the slots `,1 ,2 ,5 ,6 ,7` holds `{EACD2258-...}` or `{EC1CC9CE-...}`; `can_configure` = present and `!is_virtual_device("<Properties {a45c254e-df1c-4efd-8020-67d146a850e0},2> (<{b3f8fa53-0004-438e-9003-51a46e139bfc},6>)")`; `enhancements_off` = `Disable_SysFx` DWORD 1 in `FxProperties` (where DeviceSelector reads it) or `Properties` (where the Windows toggle writes it).

`needs_restart` is computed from timestamps, not only from a flag: true when some running `audiodg.exe` started at or before the newest of (the last change this service made, the `FxProperties` last-write time of every configured endpoint); if the start times cannot be read, only the in-memory "changed since the last restart" flag decides; with no `audiodg.exe` running it is false (the next one starts fresh). `apo_loaded` (is `EqualizerAPO.dll` in a running `audiodg.exe`) is best effort: `Some(true)`, `Some(false)` only when every `audiodg.exe` could be inspected, else `None`. It is information only (a tooltip): `Some(false)` is normal while nothing plays on an attached device (Clarim's own mic engine keeps `audiodg.exe` alive by playing into the cable), so only `needs_restart` ever asks the user to restart.

Install (service, `apo_installer.rs`): the pinned official x64 installer, `https://sourceforge.net/projects/equalizerapo/files/1.4.2/EqualizerAPO-x64-1.4.2.exe/download`, SHA-256 `7403be7427bbe1936a40dded082829b6e217fc4f5990fee5cba501f0ae055afa` (from the Chocolatey community package manifest for 1.4.2, same hash in a public ANY.RUN analysis; SourceForge publishes no SHA-256 and the file was not downloaded to double-check: a wrong value only makes the install refuse with "hash diferente"). Downloaded with `%SystemRoot%\System32\curl.exe -q -L --fail --silent --show-error --max-time 300 --proto =https` into `<Program Files>\Clarim\cache` (admin-only write); size must be 1 MB..100 MB; the SHA-256 (pure Rust, NIST vectors) is checked before anything runs and a file that does not match is deleted and never run; the installer runs with `/S` (5 min timeout, cancelled when the service stops: the service stop sets the cancel flag the moment it is seen, before the agent is terminated, the installer and its `DeviceSelector.exe` / `UpdateChecker.exe` / `Editor.exe` children are killed, and no new operation may begin). It runs whenever `installed` is false, and `installed` here means a COMPLETE install (`apo_installer::apo_ready`: `EqualizerAPO.dll` + `HKLM\SOFTWARE\EqualizerAPO` `InstallPath` + both classes registered), so a DLL left behind by an interrupted install or a failed `regsvr32` is not "installed": the dashboard shows the install button again, attaching is refused, and the installer runs over the top. Attaching a device to classes Windows cannot create would break that device's effects. Under `/S` the installer still starts `DeviceSelector.exe /i`, an interactive dialog that would sit invisible in session 0 for ever, so while the installer runs any `DeviceSelector.exe` from the install folder is killed (checked every 250 ms). Afterwards: `EqualizerAPO.dll`, `HKLM\SOFTWARE\EqualizerAPO\InstallPath` and both class registrations must exist, the installer file is deleted, and `UpdateChecker.exe -u` runs (the switch the installer itself uses when "check for updates" is cleared: otherwise a scheduled task would pop up an APO window at every logon). Custom install folders and non-x64 Windows are not handled.

Attach / detach (service, `apo_plan.rs` pure planner + `apo_reg.rs` registry layer). The planner takes the endpoint's registry state (`FxProperties` values, `Properties` combined-device flag, the `Child APOs` record) and returns an ordered `Plan` of `CreateFxKey / SetFx / DeleteFx / DeleteFxKey / CreateChildKey / SetChild / DeleteChild / DeleteChildKey`; `apo_reg::apply_plan` executes it. Rules, mirroring DeviceSelector 1.4.2 (`DeviceAPOInfo::{load,install,uninstall}`, mapping and sources in `docs/eqapo.md`):
- Mode when not installed: only LFX/GFX slots (`,1` / `,2`) and no SFX/MFX/EFX/multi (`,5 ,6 ,7 ,13 ,14 ,15`) = `LFX_GFX`; else `Properties {b3f8fa53-...},41` present (Bluetooth device shown as one combined device) = `SFX_MFX`; else `SFX_EFX` (what Realtek, USB and the Bluetooth headphones of the development PC got).
- Record `HKLM\SOFTWARE\EqualizerAPO\Child APOs\{guid}`: slots `,1 ,2 ,5 ,6 ,7` hold the original value, `!VALUE` (it did not exist) or `!KEY` (the whole `FxProperties` key did not exist); `PreMixChild` / `PostMixChild` = the original class ids APO chains to (original SFX/LFX and EFX/MFX/GFX per mode, empty = none); `AllowSilentBufferModification` = `false`; `Version` = `2`.
- `SFX_EFX` writes: delete `,1` and `,2` if present; `,5` = `{EACD2258-...}` (+ `{d3993a3f-99c2-4402-b5ec-a92a0367664b},5` = multi-sz `{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}` only if absent); `,7` = `{EC1CC9CE-...}` (+ the `,7` processing mode only if absent); `,6` untouched; delete `Disable_SysFx` (`{1da5d803-...},5`) if present. `SFX_MFX` writes `,5` and `,6` instead; `LFX_GFX` writes `,1` and `,2` and deletes `,5 ,6 ,7`. No `FxProperties` key at all: create it with the title `{b725f130-47ef-101a-a5f1-02608c9eebac},10` = `Equalizer APO` and record `!KEY`.
- Detach restores each slot from the record (`!VALUE` deletes the value, `!KEY` deletes the whole `FxProperties` key), deletes the record and the empty `Child APOs` root.
- Clarim differs from DeviceSelector in four places so that attach then detach restores the exact earlier state (unit-tested for 9 fixtures): the record carries `ClarimCreatedModes` (the processing-mode values this attach created, removed on detach while they still hold the default; DeviceSelector leaves them behind) and `ClarimSysFxWas` (the original `Disable_SysFx` DWORD, restored on detach; DeviceSelector never restores it), APO ignores both; a device that already has an APO slot but no record (an "orphan") is a no-op for attach and refused for detach, with a message that points to the Equalizer APO uninstaller (DeviceSelector would record APO as its own child); a record with a `Version` other than 2 is refused for both; a slot holding a non-string value is never overwritten.
- Safety: `apo_reg::resolve_endpoint` accepts only a braced GUID that is an existing subkey of `MMDevices\Audio\Render`; keys are opened with the minimum rights; before the first change to a device the service writes `<Program Files>\Clarim\backup\<guid>.json` (the tracked `FxProperties` values and the record, `create_new`, never overwritten; admin-only; kept by `clarim uninstall`); a plan cut off at any step is rolled back from the state read before it (`plan_restore`, unit-tested at every cut); attaching also sets `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio\DisableProtectedAudioDG` = 1, after the backup was written and before the plan runs, so a failed backup changes nothing (the value stays 1 if a later step fails: APO is installed and needs it) (DeviceSelector does too: the Equalizer APO DLL is not signed by Microsoft, and Windows only loads it into the protected audio process with that value). The service refuses `install_apo` for more than 16 devices.

Restart Windows Audio (service, `audio_restart.rs`): the running services that depend on `Audiosrv` are stopped (enumerated with `EnumDependentServicesW`, two passes for chains), `Audiosrv` is stopped (25 s) and started, then the dependents are started again in reverse order. `Audiosrv` is started with retries: from STOPPED `StartServiceW` is called again about every 2 s until the 25 s deadline (a start that was accepted and fell back to STOPPED, or was refused, gets another try, as Equalizer APO's own `ServiceHelper` does; START_PENDING is only waited out; `start_step` is the pure rule). A failure before `Audiosrv` is stopped puts the dependents back and returns a PT-BR error that tells the user to restart the PC; so does an `Audiosrv` that will not start after the retries (the dependents are started anyway: the SCM starts `Audiosrv` first for a dependent, one more attempt, and the services that were running are not left down); dependents that do not come back are a warning in `error`. When the Clarim service is stopped (or upgraded) the operation slot is closed at once (`begin` refuses with a PT-BR text), a running installer is cancelled, and the service waits up to 8 s (`APO_STOP_WAIT`, inside the 12 s StopPending wait hint and the 15 s `stop_and_wait` of install/uninstall) for the running operation to end before it reports Stopped, so the process does not exit between the stop and the start of `Audiosrv`; there is no cancel inside `restart_audio` itself. The sequencing is tested through a `Services` trait with a fake; real restarts are rate-limited to one per 10 s. The sound drops for a few seconds, and an app that holds a stream may need to be reopened.

Agent side (`apps/clarim/src/eqapo.rs`, `backend.rs`): the three `Backend` commands are refused with a PT-BR message under `--dry-run` and when the service pipe is not there (installing, attaching and restarting need LocalSystem). `install_eq_apo` sends the endpoint GUIDs of the distinct physical devices the four channels play on, in channel order (`install_targets`: a channel on the Windows default or on an unplugged device counts as the default device; virtual devices - `is_virtual_device`, `cable`, `vb-audio`, `voicemeeter` - and the deny list are never sent); with none, only APO is installed. `set_eq_device` takes an endpoint id, sends its GUID, and for attach refuses (before the service does) a device that is not in the current list, is virtual or is denied; detach is always forwarded. After each command the agent reads the service status again at once (so `busy` / `needs_restart` show in the next `/api/state`), refreshes are serialized, and the first write of `clarim-eq.txt` waits until the service is not busy (the installer is still writing `config.txt`).

Audio recovery after a restart (or a crash) of Windows Audio: the mic engine recognises `AUDCLNT_E_DEVICE_INVALIDATED`, `AUDCLNT_E_SERVICE_NOT_RUNNING`, `AUDCLNT_E_RESOURCES_INVALIDATED`, `AUDCLNT_E_ENDPOINT_CREATE_FAILED`, `ERROR_SERVICE_NOT_ACTIVE` and the RPC unavailable/failed/disconnected HRESULTs (`clarim_audio::is_audio_service_loss`) and reports the soft note `Reconectando ao áudio do Windows (...)` (`clarim_audio::RECONNECTING`, a warn-level log) instead of an error; the dashboard keeps that note out of the red chip (the chip reads "Reconectando o áudio…") and the agent's mic retry stays at 3 s while the note is up (`Retry::bump(reconnecting)`), instead of backing off to 30 s. The routing worker counts consecutive bad scans (enumeration failed, no endpoints, or every per-app lookup failed); after 3, and then every 5th, it makes a new `IMMDeviceEnumerator` and a new policy factory (keeping the old one when a new one cannot be made), clears its per-pid counters, levels and live sessions, and logs when Windows Audio is back. The routes Windows persisted for each app survive the restart.

## clarim-audio

Public API:

```rust
pub fn init_com() -> anyhow::Result<()>;                      // MTA
pub fn list_inputs(deny: &[String]) -> anyhow::Result<Vec<DeviceInfo>>;
pub fn list_outputs(deny: &[String]) -> anyhow::Result<Vec<DeviceInfo>>;
pub fn find_virtual_mic(outputs: &[DeviceInfo]) -> Option<&DeviceInfo>;   // order in "Design decisions"
pub fn pick_real_mic(inputs: &[DeviceInfo]) -> Option<&DeviceInfo>;       // default if not virtual, else first non-virtual

pub struct MicEngine { /* owns threads */ }
impl MicEngine {
    pub fn new(deny: Vec<String>) -> Self;          // idle
    pub fn apply(&mut self, s: &MicSettings);       // start/stop/restart streams if devices or enabled changed; else push live DSP params (no glitch)
    pub fn status(&self) -> MicStatus;
    pub fn start_calibration(&self, seconds: f32);  // collect speech frames (VAD > 0.6) RMS; on finish result via take_calibration()
    pub fn take_calibration(&self) -> Option<f32>;  // Some(dBFS) once, after calibration completes (70th percentile of speech-frame RMS dB)
    pub fn counters(&self) -> Option<Counters>;     // (underruns, overruns, slips) since the streams started; None while not running
    pub fn stop(&mut self);
}
pub fn selftest() -> anyhow::Result<String>;        // synthetic speech+noise through every DenoiseMode; returns report incl. µs/frame and, for the auto_level mode, the AGC gain and the output speech level; no devices touched
```

Chain order per 10 ms frame: input gain → HPF 80 Hz (2nd-order biquad, Q 0.707) → denoise (off / RNNoise / DFN3) → Voice Focus expander → noise gate → EQ preset → tone sliders Graves/Médios/Agudos (low shelf 150 Hz, peak 1.5 kHz Q 1, high shelf 6 kHz, ±12 dB each, bypassed at 0) → auto level (voice AGC, `dsp/agc.rs`) → output gain → limiter −1 dBFS (the gain sits before the limiter so a boost can never pass the ceiling). Monitor gets the same output.

Streaming and known limits (`pipeline.rs`, `pacing.rs`):
- No polling: capture wakes DSP after each packet, DSP wakes the render thread(s) after each block, a tick thread wakes a render thread at each audio-engine tick. Capture, DSP, render and tick threads register with MMCSS ("Pro Audio"), falling back to TIME_CRITICAL; the DSP thread flushes denormals (FTZ/DAZ).
- The render thread writes each block as soon as it exists and steers the TIME-AVERAGED queue (endpoint padding + ring) to one engine period + 3 ms (grows by 2 ms per jitter underrun, up to 20 ms) by slipping at most one sample per write (neighbour merge/insert, not a cut; corrects up to ~2000 ppm). A capture/DSP/render stall drops the stale backlog once, an empty endpoint is rebuilt with silence plus a 2 ms fade-in.
- Underruns are inferred from the engine ticks counted by the tick thread since the render thread's previous wake: the endpoint is empty and the queue the previous wake left (`last_total`) is smaller than `ticks x period`. That sees a whole missed period, also when the endpoint held 0 frames or an exact multiple of the period; a wake from the DSP bell (`ticks == 0`) never counts. The tick count is read before the padding query, so a race can only miss an event, never invent one. Each stream's event handle is closed after the stream stops (`EventCloser`; `wasapi::Handle` never closes it, and the HANDLE is read back under compile-time size and no-Drop asserts).
- `MicStatus.latency_ms` is an ESTIMATE of our own mic-to-cable delay (half a capture period + capture backlog + denoiser delay + DSP time + the averaged render queue). It excludes the audio engine's mixing delay, the cable driver and the buffers of the app reading the cable. Counters (underruns, overruns, slips) are in `MicEngine::counters()` and, every 10 s, in the debug log (warn when glitches grew).
- `voice_level_db` is stored at unity input gain (calibration subtracts `input_gain_db`); the Voice Focus threshold is `voice_level_db + input_gain_db + voice_focus_db` (uncalibrated: −26 + `voice_focus_db`).
- "Ouvir meu microfone": the monitor render thread exists only while `monitor_on` (toggling restarts the streams); `monitor_id = None` means the Windows default output, or the first real non-denied one when that is virtual, never the cable. The device is resolved once, at engine start. If none is found, or its stream fails to open or stops later (headphones unplugged), the mic and cable keep running and `MicStatus.error` carries the soft note "Ouvir meu microfone indisponível..." (only when nothing else is wrong). It is never retried by itself, because the only restart is the whole engine and that would cut the mic in a call: the user toggles the option off and on.
- Known limit: the first switch to `strong` in a session builds the DeepFilterNet model on the DSP thread, a ~0.5 s stall (an audio gap, visible as one underrun and a few overruns in the counters); later switches reuse the model. Windows delivering only silence-flagged packets for 2 s (privacy switch) shows a soft note in `MicStatus.error`.

- **DFN3**: hop must be 480 @ 48 kHz (verify in research). Its algorithmic latency (~2 lookahead frames) is acceptable. Build the DFN model once per engine start on the DSP thread (it is not cheap); if construction fails, log, set `denoise_active = Light`, use RNNoise. `denoise_strength` → DFN `atten_lim_db`. Speech probability: use DFN lsnr mapped to 0..1 (document mapping) or run RNNoise VAD in parallel only if cheap; pick one, explain in a comment.
- **Voice Focus**: RMS envelope (attack ~5 ms, release ~120 ms) of post-denoise signal. Open when `env_db > voice_level_db + voice_focus_db` AND vad > 0.5; hold 250 ms; closed gain = −30 dB floor (not total silence, avoids pumping artifacts), smoothed gain changes (attack 3 ms, release 80 ms). Unit-test: loud speech-like signal passes, same signal −20 dB is attenuated by ≥ 15 dB when threshold sits between.
- **EQ presets** (biquads): Flat; Clarity (+3 dB high-shelf 4 kHz, −2 dB peak 300 Hz); Warm (+2.5 dB low-shelf 150 Hz, −1.5 dB high-shelf 8 kHz); Broadcast (+2 dB low-shelf 120 Hz, −3 dB peak 400 Hz Q1, +3 dB peak 3.5 kHz Q1).
- **Auto level = voice AGC** (`dsp/agc.rs`, `Agc`; replaces the old compressor; full design and tests in `docs/leak-agc.md`). Target `AGC_TARGET_DB` = −23 dBFS speech level, measured as the dB mean of 10 ms frame RMS over learned speech frames (the synthetic `selftest::Voice` reads about 3.3 dB under its power RMS). Gain `AGC_MIN_GAIN_DB` −12 .. `AGC_MAX_GAIN_DB` +20, raised at most 3 dB/s and only during speech, lowered at 10 dB/s at any time, applied as a per-sample linear ramp across each frame. It learns only from speech (RNNoise VAD ≥ 0.6 for 12 adjacent frames, frame level above −60 dBFS); once 40 frames are learned, frames 20 dB or more below the estimate (leaked or distant voices, focus-muted frames) are ignored, louder ones always learned. It never lifts the post-denoise noise between words above −55 dBFS (gain cap `max(−55 − noise_in, 0)`). A limiter guard cuts the gain by 0.3 dB per frame when the peak after the AGC and the output gain would pass −3 dBFS (also before it is confident). Learning keeps running with `auto_level` off; switching it off ramps the gain to 0 at 0.3 dB per frame, then the block is a bit-exact bypass. `shift_db` moves the learned levels by a known input change (`input_gain_db`, or the Windows mic volume through `EngineCmd::MicVolumeShift`). `FrameInfo` and the meters carry `agc_gain_db`, `input_voice_db` and `voice_db`. No allocation, lock, syscall or logging on the DSP thread.
- **Leak detection** (`dsp/leak.rs`, `refs.rs`; the DSP and thread design is in `docs/leak-agc.md`). One WASAPI loopback thread per physical render device (max 4, running only during calls with `leak_warnings` on; they outlive mic restarts and are never joined under the `mic` mutex) publishes the mean square of every 10 ms block into lock-free slots. The DSP thread reads each slot (fixed 2 blocks behind the newest) and feeds one `EchoDetector` per slot (WebRTC's residual echo detector in the dB domain: normalised covariance over 80 lags, 0..790 ms; learned only on output frames at least 10 dB under the user's speech level; a peak at 0-20 ms or at the window edge is never a leak) and a `NoiseFloor` (P20 of the non-speech output levels, median of the last 10 s). It also counts clipped raw input speech frames (`clip_events`: at least 2 samples with |s| ≥ 0.985, or 8 bit-identical consecutive samples with |s| ≥ 0.25, while VAD ≥ 0.5). Known limits: the call app may already cancel its own playback downstream (Discord, Meet/Chrome), so the echo warning matters most for other apps' sound; echo within 10 dB of the user's voice is not scored; balloons wait while Windows says the user is busy.
- Device hot-plug / errors: when a stream thread errors, status shows the error (a Windows Audio loss shows the soft note "Reconectando ao áudio do Windows...", see "Audio recovery after a restart"); `apply()` with the same settings retries. The agent calls `apply()` every ~3 s while `running == false && enabled` (auto-recover after unplug/replug).

## clarim-routing

```rust
pub struct RoutingEngine { /* worker thread with COM MTA */ }
impl RoutingEngine {
    pub fn start(cfg: RoutingConfig) -> Self;
    pub fn update(&self, cfg: RoutingConfig);         // non-blocking (channel send)
    pub fn sessions(&self) -> Vec<AppSession>;        // last enumeration (≤1 s old, ≤2 s while routing is off); channel = EFFECTIVE channel
    pub fn peaks(&self) -> Vec<(u32, f32)>;           // ≤100 ms old while somebody polls (see Worker loop)
    pub fn captures(&self) -> Vec<CaptureSession>;    // capture sessions of the last scan (all apps, all capture endpoints; empty when the render enumeration failed); never counts as a poll (does not touch the idle stamp)
    pub fn stop(self);
}
#[derive(Clone)]
pub struct RoutingConfig {
    pub channels: BTreeMap<ChannelId, ChannelSettings>,
    pub app_rules: BTreeMap<String, ChannelId>,
    pub default_channel: ChannelId,
    pub enabled: bool,          // false → enumerate/meter only, never call SetPersistedDefaultAudioEndpoint / SetMasterVolume / SetMute (one exception: the true → false switch releases what this run wrote, see Worker loop)
    pub call_to_chat: bool,     // Config::call_to_chat: Media apps capturing a mic play on Chat (AppSession.in_call) until 20 s after the call
}
pub fn clear_all_app_routes() -> anyhow::Result<()>; // ClearAllPersistedApplicationDefaultEndpoints — used by `clarim reset-routing`
```

Policy API facts (from docs/research.md, EarTrumpet MIT / Earmark MIT — write original code, don't copy): activation class `Windows.Media.Internal.AudioPolicyConfig` via raw `RoGetActivationFactory` imported from `combase.dll` (windows-rs' generic wrapper can't take a custom IID). IID for build ≥ 21390 (Win11, this machine is 22631): `ab3d4648-e242-459f-b02f-541c70306324`. Older builds (all of Windows 10) use another IID whose vtable layout is unverified, and a wrong slot could crash the process, so `Policy::new` refuses builds < 21390 and routing runs volume-only there (one `log::warn!`). vtable: IUnknown 0–2, IInspectable 3–5, 19 placeholders 6–24, `SetPersistedDefaultAudioEndpoint(this, pid: u32, flow: i32, role: i32, device: HSTRING) -> HRESULT` = slot 25, `GetPersistedDefaultAudioEndpoint(this, pid, flow, role, *mut HSTRING)` = 26, `ClearAllPersistedApplicationDefaultEndpoints(this)` = 27. Device string = `\\?\SWD#MMDEVAPI#` + `IMMDevice::GetId()` + `#{e6327cad-dcec-4949-ae8a-991e976a79d2}` (render). Null HSTRING = clear (follow Windows default). Set roles eConsole(0), eMultimedia(1) and eCommunications(2). Create the factory once on the worker thread, Release it on drop. Get the OS build via `RtlGetVersion` (ntdll) or registry `CurrentBuildNumber`.

**Never pass a device id that is not currently present** (an invalid id can silence the app): if a channel's `device_id` isn't among active render endpoints, treat it as `None`.

Worker loop: every 100 ms update peaks (IAudioMeterInformation of known sessions) **only while somebody polls** `sessions()` / `peaks()` (the open dashboard); after 3 s without a call the worker ticks every 500 ms and publishes no peaks, so a closed dashboard costs nothing. Scan (enumerate all active render endpoints → sessions; skip system-sounds session, our own PID, PID 0, and audio plumbing: `audiodg.exe`, `steelseriesggclient.exe`, `nvidia broadcast.exe`) every 1 s while routing is on, every 2 s while it is off, again after 250 ms when a route was written, and immediately after `update`. Then resolve the **effective channel** (rule or default channel; with `call_to_chat`, a Midia app that is in a call is Chat) → if enabled:
- read `GetPersistedDefaultAudioEndpoint(pid, eRender, role)` for eMultimedia and eCommunications; if either differs from the desired device, `Set…` for all three roles. Max 3 attempts per (pid, desired device) so a process that refuses never causes a write loop (warned once per target). Forget dead PIDs. Chromium/Electron play audio from a utility subprocess with the same exe name — routing that PID is correct; resolve exe via `QueryFullProcessImageNameW` (OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION; access denied → exe "unknown", skip routing). `AUDCLNT_S_NO_SINGLE_PROCESS` from GetProcessId is success. System-sounds session → skip.
- if channel volume/mute differs from what we last applied to that session: `ISimpleAudioVolume::SetMasterVolume/SetMute`. Re-apply only on change, so a user tweak in the Windows mixer survives until the channel slider moves. A session of an exe Clarim has not written in this run (`touched_exes`, never pruned, not cleared by the release) is left alone while its channel is at the defaults (100 %, unmuted); otherwise, and always for an exe Clarim did write, it is compared with its current state first (Windows keeps the level per exe and starts every new session of it there, so a new tab, a restarted app or a promoted call may start at a stale level of ours). A stale level an earlier Clarim run left is not corrected: `touched_exes` starts empty on each run.
- Capture sessions: every scan lists the capture sessions (eCapture endpoints, same skips as above), whatever `call_to_chat` is, and publishes them through `captures()` (the agent's call detection reads them; the list is replaced by an empty one when the render enumeration fails, so a call cannot stay alive while Windows Audio is down). The scan runs every 2 s while routing is off or observe-only, so the call state can lag up to 3 s. The enumerated list is shared with the promotion below, not enumerated twice.
- Call detection (promotion; only with `call_to_chat`): the same captures are remembered, per exe, with when one was last active. A Midia app (by rule or as the default channel) that captured at most 20 s ago (`CALL_HANGOVER`) has `in_call = true` and Chat as its effective channel, for its device, volume, mute and `AppSession.channel`. Other channels are never promoted; promotion is per exe, not per tab. `clarim sessions` has no history, so it only shows apps capturing right now.
- Turning routing off (`enabled` true → false) releases this run's footprint once. The footprint is keyed by exe (Windows stores the per-app route and volume under the exe path, so a restarted app or a respawned browser audio process inherits them without being a write of its own) and is not pruned while the engine runs: the route is cleared through every live pid of each exe Clarim routed, and the sessions of each exe whose volume/mute Clarim wrote are reset to 100 % / unmuted (only the parts we wrote). Programs with no live session (closed apps) cannot be reached, the policy API needs a pid. Nothing is released on exit, when the engine starts disabled, or for routes Clarim did not write (`clarim reset-routing --yes` covers those).
- When Windows Audio restarts under the worker (3 consecutive bad scans, then every 5th) it rebuilds its `IMMDeviceEnumerator` and policy factory and drops its per-pid state (see "Audio recovery after a restart").
- All COM failures are logged at debug and skipped; the loop never panics. If the policy factory can't be created (unsupported Windows build), routing degrades to volume-only and reports it via `log::warn!` once.
Unit tests: device-id string wrapping/unwrapping for the policy API; channel resolution; call promotion and hangover; first-sight volume (untouched and touched exes); footprint and release targets per exe; idle detection. `cargo test -p clarim-routing live_sessions -- --ignored --nocapture` prints the raw capture/render sessions of the machine (read-only).

## clarim-web

```rust
pub fn serve(backend: Arc<dyn Backend>, port: u16) -> anyhow::Result<std::thread::JoinHandle<()>>; // binds 127.0.0.1:port only
```

Security: reject any request whose `Host` header is not `127.0.0.1:<port>` or `localhost:<port>` (DNS rebinding). Generate a random 128-bit token at `serve()` start (no new deps: e.g. two `std::collections::hash_map::RandomState` hashers, which are OS-seeded), inject it into the served HTML (replace a `__CLARIM_TOKEN__` placeholder). Every `/api/*` request (GET included) requires header `X-Clarim-Token: <token>`, compared in constant time, and, if an `Origin` header is present, it must be `http://127.0.0.1:<port>` or `http://localhost:<port>`. Browsers can't add custom headers cross-origin without a CORS preflight, which we never answer. No CORS headers ever, `OPTIONS` → 405. Bodies capped at 64 KB. Responses get `Cache-Control: no-store`.

Routes (JSON, UTF-8):
- `GET /` → dashboard.html; `GET /favicon.svg` → small inline SVG
- `GET /api/state` → `Snapshot`
- `GET /api/meters` → `Meters` (dashboard polls ~10 Hz while visible)
- `POST /api/mic` body `MicSettings` → `{ok:true}` | 400 `{error}`
- `POST /api/channel` body `{ "id": "chat", "settings": ChannelSettings }`
- `POST /api/output_eq` body `{ "device_id": "{0.0.0.00000000}.{GUID}", "eq": OutputEq | null }` (`null` removes the device's EQ; the `eq` key is required). 400 when `device_id` is blank or longer than 512 chars, or a gain is NaN/inf; gains are clamped to ±12 dB; the backend refuses an id that is not an endpoint id with a GUID (`endpoint_guid`)
- `POST /api/eq_apo/install` (no body) -> the Clarim service installs Equalizer APO when missing and attaches it to the physical devices the channels use; `{ok:true}` as soon as the service accepted the job, then `eq_apo.busy` follows it. 400 `{error}` (PT-BR) under `--dry-run`, without the service, or while another operation runs
- `POST /api/eq_apo/device` body `{ "device_id": "{0.0.0.00000000}.{GUID}", "enabled": bool }` (both keys required) -> attach / detach Equalizer APO on one device. 400 when `device_id` is blank or longer than 512 chars; the backend refuses a device that is not an endpoint id, not listed, virtual or denied (attach only)
- `POST /api/audio/restart` (no body) -> the service restarts Windows Audio (the sound drops for a few seconds); `{ok:true}` once accepted
- `POST /api/assign` body `{ "exe": "spotify.exe", "channel": "media" | null }`
- `POST /api/routing` body `{ "enabled": bool }`
- `POST /api/calibrate` body `{ "seconds": 5 }`
- `POST /api/refresh` → re-enumerate devices

Dashboard (`crates/clarim-web/src/dashboard.html`, PT-BR, dark theme, responsive, no external requests — no CDN, no web fonts):
- Header: "Clarim", status chip (Mic ativo / erro), toggle "Roteamento por app".
- **Microfone** card: Entrada select, Microfone virtual (saída) select (+ warning with VB-Cable link https://vb-audio.com/Cable/ when none found), live meters Entrada/Saída + "Voz detectada" dot, Supressão de ruído IA segmented [Desligado | Leve | Forte] + Intensidade slider, Foco na voz toggle + Sensibilidade slider + "Calibrar minha voz (5 s)" button with progress, Filtro de graves toggle, Volume automático toggle (the old "Nivelamento automático"; hint "Mantém sua voz no mesmo volume na chamada: não estoura nem fica baixa."; while on, a `.mini` readout under it, "ganho +6 dB · voz −23 dBFS", "voz —" when `voice_db` is null), Avisar se vazar som na chamada toggle (`leak_warnings`, in the "Voz" column), Ganho de entrada slider, Ouvir meu microfone toggle + fone select. The mic equalizer sits in the "Tom e nível" column, always visible: Equalizador select (Neutro/Clareza/Quente/Locutor) followed by Graves/Médios/Agudos sliders (-12..+12 dB, step 1, readout like "+3 dB", applied after the select's preset). A closed-by-default **Avançado** section holds Noise gate toggle + limiar, Ganho de saída slider, the toggle "Ajustar o volume do microfone no Windows se estourar" (`auto_mic_volume`; disabled while `auto_level` is off), a `.mini` debug line shown only while `in_call` ("Vazamento: eco 0,12 · atraso 250 ms · ruído −62 dBFS · referências 1") and the small "DSP x µs · latência ~y ms" line. The calibration result reads "Calibrado" / "Não calibrado" (the dB value is a tooltip); with Foco na voz on and no calibration the button carries a "sem calibrar" mark. While `in_call`, the card header shows the pill "Em chamada: Discord" (`call_apps` without `.exe`, first letter upper-cased, joined with ", ").
- **Canais**: 4 columns (Jogo / Chat / Mídia / Aux): output device select (never lists the virtual-mic endpoint the mic writes into), volume slider (0–100), mute button, list of apps currently in the channel with their peak bar; each app has a channel select to move it (writes a rule; "Sem regra (canal padrão)" removes it). Unassigned apps show under the default channel. An app with `in_call` shows in Chat with an "em chamada" tag (its select still shows the rule); an active app whose `device_id` differs from its channel's explicit, plugged-in device while routing is on shows "não roteado" (a channel on the Windows default is never flagged: each app follows the default of the role it opens, and the console and communications defaults can differ). Layout: the page scrolls vertically when it must (no fixed-height page), and each channel's app list has room for at least 4 rows (190 px, up to 380 px) before it scrolls inside the card; no horizontal overflow at 700 and 420 px.
- **Equalizador de saída** card (under Canais; one EQ per playback DEVICE, see "Output equalizer (Equalizer APO)"; Clarim installs and configures Equalizer APO, the user only presses buttons here and never opens Equalizer APO's installer, DeviceSelector or Editor, and the page never tells them to). From the top: a busy banner with `eq_apo.busy` (spinner glyph) while the service works; a warning banner with `eq_apo.error` (also while APO is missing: a failed install, no service, `--dry-run`); while `eq_apo.installed` is false the install box: "Para equalizar o que você ouve, o Clarim instala o Equalizer APO (grátis) por você.", the note "Baixa cerca de 12 MB de sourceforge.net (link), leva ~1 min e o som pode piscar." and ONE button "Instalar equalizador" (disabled unless `writer == "service"` and nothing is running; its title says why); while `needs_restart` (installed) the warning "Falta reiniciar o áudio pra valer: a mudança no equalizador só vale depois disso. O som some por uns segundos." (state-neutral: `needs_restart` is also true right after a device was switched OFF, when the effect still plays until the restart) with the button "Reiniciar áudio agora" (same enabling rule); otherwise, once installed, the controls: Aparelho select (real, non-virtual outputs; the ones the channels play on first, labelled with those channels; a channel on the Windows default or on an unplugged device counts as the default device), the switch "Equalizador neste aparelho" (enabled when the service is free and the device is not `can_configure == false`; flipping it posts `/api/eq_apo/device` and shows the new state at once, the next state confirms it), a status line, four preset buttons Neutro / Mais graves / Clareza / Agudos (`aria-pressed` plus a check mark; 0/0/0, +5/0/0, -1/+2/+3, 0/0/+4 dB bass/mid/treble) and Graves/Médios/Agudos sliders (-12..+12 dB, step 1). Presets and sliders are enabled only for a device the equalizer is on for and post `/api/output_eq` for it (sliders debounced 200 ms, a preset or a released slider at once). Status line: "Ativo neste aparelho" (attached; tooltip "O Windows já carregou o Equalizer APO." when `apo_loaded` is true); "Ligado neste aparelho. Só vale depois de reiniciar o áudio" while `needs_restart`; a warning when `enhancements_off` lists the device or the device is refused; "Desligado neste aparelho. Se estava ligado, ainda toca até reiniciar o áudio (aviso acima)." while `needs_restart` and the device is off (the flag is global, so the text hedges); "Desligado neste aparelho. Ligue a chave acima..." when off otherwise; with only the direct writer a hint that the switch needs the service; no claim when the state is unknown; a Bluetooth note when the device name matches `bluetooth` or `BT`. `writer == "none"` disables the select, the switch and the controls. Every system string (busy text, error, device names) is set with `textContent` / `createElement`, never `innerHTML`. After install, switch or restart the page posts, then reads `/api/state` at once.
- **Notices**: a (legacy) SteelSeries Sonar output device shows a warning (routing on: the two fight over each app) or a hint (routing off; it names no cause, since routing may be off by the user's choice or by `--no-routing`, and the generic "só observa" note stays visible); a running mic whose `in_peak` stays exactly 0 for about 5 s, or the engine's "Microfone silenciado..." note, shows ONE yellow "Sem sinal do microfone..." banner; the engine's soft notes in `mic.error` never turn the status chip red or get the red error banner: calibration notes ("Calibração...") are toasts only, "Microfone silenciado..." is the banner above, "Ouvir meu microfone indisponível..." is a yellow banner under Fones, "Reconectando ao áudio do Windows..." (Windows Audio is restarting) only makes the chip read "Reconectando o áudio…"; the mic error banner is not repeated while the VB-Cable warning is shown. Call notices (all text through `textContent`, never `innerHTML`; none of them is a red error): while `mic.in_call && mic.leak` a yellow `#leak` banner (`role="status"`), echo = "Tem som das caixas vazando no seu microfone: o pessoal da chamada pode ouvir o jogo, vídeos ou outros sons daí. Abaixe o volume das caixas ou use fone.", noise = "Tem ruído de fundo passando pelo filtro na chamada. Aproxime o microfone ou ligue o Foco na voz." plus " Tente também o filtro Forte." unless `denoise` is `strong`; `mic.mic_volume_note` in a blue `#micvol` banner; while `mic.mic_too_quiet` a `#quiet` hint line, "Sua voz está chegando baixa mesmo com o volume automático no máximo. Suba o volume do microfone no Windows ou aproxime o microfone.". The meters key that decides a repaint includes `in_call`, `leak`, `mic_volume_note`, `mic_too_quiet`, `call_apps`, the rounded `agc_gain_db`, `voice_db` and `noise_db`, `echo_score` in percent and `references`. The tray balloons and tooltip are in "Call tick".
- Changes post immediately (debounce sliders ~150 ms). Poll `/api/state` every 2 s, `/api/meters` every 100 ms while `document.visibilityState === 'visible'`. Never clobber a control the user is dragging.
- Keyboard accessible, labels on every control, text contrast ≥ 4.5:1, color not the only signal.

Tests: spin `serve` on an ephemeral port with a mock Backend; check Host rejection, missing-header rejection on POST, and a successful POST round-trip.

## clarim-svc

```rust
pub const SERVICE_NAME: &str = "Clarim";
pub fn install() -> anyhow::Result<()>;   // needs admin: copy current exe to <Program Files>\Clarim\clarim.exe (FOLDERID_ProgramFiles via SHGetKnownFolderPath, not the environment), register service (auto start, LocalSystem, recovery: restart after 5 s ×3), start it, create the all-users Start-menu shortcut `Clarim.lnk` -> the installed exe with no arguments (one PowerShell WScript.Shell call; a failure there only logs a warning). Quoted binary path: "\"C:\\Program Files\\Clarim\\clarim.exe\" service". Once it has stopped (or tried to stop, up to 30 s) an existing service, any later failure (kill, copy, change_config, start...) first starts that service again, best effort, then returns the error; the exe is copied to `clarim.exe.new` in the same folder and renamed over the installed one, so a copy that fails half way never damages the exe that the restart needs; the copy and the rename retry every 250 ms for up to 10 s on a sharing violation / access denied / lock violation / user-mapped section still open. `install` and `uninstall` log (and write the final error with its full chain) to `<Program Files>\Clarim\install.log` (cut back to empty above 1 MB; `uninstall` keeps it), mirrored to stderr, so an elevated run with no console leaves a record
pub fn uninstall() -> anyhow::Result<()>; // stop, delete service, remove the shortcut, take the Clarim equalizer out of Equalizer APO (our `Include:` line and clarim-eq.txt; config.txt.clarim-backup stays; skipped when APO is not installed), kill agents, remove Program Files dir (best effort) but keep `<Program Files>\Clarim\backup` (the saved original settings of every device Equalizer APO was attached to). Equalizer APO itself and the devices it was attached to are NOT touched: undo that with the per-device switch, or uninstall Equalizer APO from Windows Apps
pub fn run_service() -> anyhow::Result<()>; // entry for `clarim service` (service_dispatcher)
pub fn is_installed() -> bool;
pub fn eqapo_send(entries: &[(String /* endpoint GUID "{...}" */, OutputEq)]) -> anyhow::Result<()>; // agent side of the output-EQ pipe: the COMPLETE list (the file is rewritten from it); errors are PT-BR, ready for EqApoStatus.error
pub fn eqapo_service_available() -> bool; // the pipe exists (an instance is listening, or all are busy)
// Equalizer APO management (see "Equalizer APO management"); all blocking, all errors PT-BR:
pub fn apo_status() -> anyhow::Result<ApoServiceStatus>;               // cheap; ApoServiceStatus { installed, busy, error, needs_restart, apo_loaded, devices: Vec<EqApoDevice> }, all #[serde(default)]
pub fn apo_install(device_guids: &[String]) -> anyhow::Result<()>;      // returns once the service accepted the job; max 16 GUIDs
pub fn apo_set_device(guid: &str, enabled: bool) -> anyhow::Result<()>; // synchronous
pub fn apo_restart_audio() -> anyhow::Result<()>;                       // returns once accepted
pub const EQUALIZER_APO_VERSION: &str = "1.4.2"; pub const EQUALIZER_APO_DOWNLOAD_MB: u32 = 12;
```

Output-EQ pipe server (`eqapo.rs`, runs inside the service; protocol in "Output equalizer (Equalizer APO)", Writer A, and the commands in "Equalizer APO management", dispatched by `apo_proto::handle` to the `ApoService` of `apo_service.rs`, one operation at a time, long ones on worker threads; replies up to 64 KiB): started once the service reports Running and dropped after the supervisor returns; a failure to create the pipe is logged and the service keeps supervising (agents then report writer `direct` or `none`). Byte mode, overlapped I/O, `PIPE_REJECT_REMOTE_CLIENTS`, first instance created with `FILE_FLAG_FIRST_PIPE_INSTANCE`; requests are served one at a time (the next instance is created before the current client is served); the server waits 3 s for the request line and 3 s for the reply write, and a service stop interrupts every wait. The client connects with identification-level impersonation only and waits 2 s to connect, 3 s to write and 10 s for the reply. A request carries endpoint GUIDs and gains only; the file text is `render_eqapo_file`, the folder is `<Program Files>\EqualizerAPO\config` from the shell, the file names are constants (`clarim-eq.txt`, its `.tmp`, `config.txt`, `config.txt.clarim-backup`). A `config.txt` that is not UTF-8 (e.g. UTF-16 from Notepad) is refused and left untouched; a missing config folder is not created.

Supervisor: on start and on `SESSION_LOGON`/`CONSOLE_CONNECT` session changes, launch `clarim.exe agent` into the active console session (WTSGetActiveConsoleSessionId → WTSQueryUserToken → CreateEnvironmentBlock → CreateProcessAsUserW, desktop `winsta0\default`, CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW). Watch the process handle; relaunch if it exits (backoff 2 s → 30 s max, reset after 60 s of uptime; exit code 0 = quit on purpose, never relaunched; exit code 3 = another agent already runs, relaunched at the 30 s cap). On `SESSION_LOGOFF` let it die without relaunch until next logon. On service stop: report StopPending at once (checkpoint 1, wait hint 5 s), terminate the agent, report stopped. Never launch more than one agent per session (the agent also enforces single instance via a named mutex `Local\ClarimAgent`).

## apps/clarim (integration)

Subcommands:
- `clarim` (no args): if an agent already runs (mutex exists) → open dashboard; else run the agent in-process (dev mode).
- `clarim agent`: run the agent (what the service launches). Exits with code 3 if another agent holds the mutex (and fails, not "already running", if the mutex cannot be created).
- `clarim open`: open dashboard (`msedge.exe --app=http://127.0.0.1:<port>/ --window-size=1180,820` from `%ProgramFiles(x86)%\Microsoft\Edge\Application` or `%ProgramFiles%\...`, against the user's normal profile: no `--user-data-dir` and no sync/extension switches, because a private profile signs in to the Windows account and syncs extensions; fallback ShellExecuteW "open" on the URL).
- `clarim install` / `clarim uninstall` (admin) / `clarim service` (SCM only)
- `clarim devices` — print endpoints; `clarim sessions` — list apps with an audio session (read-only; channel = effective channel, an app capturing a mic right now while in Mídia is tagged `[in call: plays on chat]`); `clarim selftest` — DSP bench; `clarim reset-routing` — clear all per-app routes; only with `--yes` (no stdin prompt: the GUI-subsystem exe would race the shell for console input), otherwise it prints the PT-BR warning, tells the user to run `clarim reset-routing --yes` and exits 1. With `--yes` and an agent running (`instance::agent_running()`), it adds a PT-BR note after the success line: a running agent with routing on writes its channel routes again within seconds, so the user must turn "Roteamento por app" off (or quit from the tray) first and run the command again.
- After `clarim install` it prints: "Clarim instalado. Ele inicia sozinho quando você entra no Windows. Abra o painel pelo menu Iniciar (Clarim) ou pelo ícone na bandeja."
- `clarim open` refuses (error) when no agent runs, instead of opening a dead page.
- `--dry-run` flag for `agent`: routing observe-only, mic engine disabled and the output equalizer off (`set_output_eq` is refused with a PT-BR message and `eq_apo.writer` is `"none"`), for testing on a real machine without touching anything. `--no-routing`: routing observe-only, mic on. Both are runtime overrides, never saved: the startup log line prints `routing_enabled` / `mic_enabled` as in force (after the overrides), and the dashboard snapshot shows what is in force (routing off, and mic off under `--dry-run`), and a change that would break the override is refused with a PT-BR error.
- **Exit-code contract (agent ↔ service supervisor)**: the agent exits 0 only for "quit on purpose" (tray *Sair*); the supervisor does not relaunch on 0. Exit code 3 = "another agent holds the mutex"; the supervisor relaunches it at the 30 s backoff cap. Any other failure (dashboard port taken, dashboard server died, mutex not creatable, message loop broken) exits 1 and is relaunched with the normal backoff. The dashboard port is bind-checked before the engines start, so a busy port never turns the mic or routing on in a restart loop. Without a tray icon the agent keeps running, and ends with a failure only if the dashboard server dies.

Agent: load config (apply first-run device defaults), start MicEngine + RoutingEngine + web server, tray icon (menu: "Abrir painel", "Pausar microfone"/"Ativar microfone", "Sair"; left click opens dashboard), Win32 message loop on the main thread. A housekeeping thread (1 s): poll calibration result → save config; retry mic if errored (3 s, doubling to 30 s, so a missing cable does not flood the log; fixed at 3 s while the engine says it is reconnecting to Windows Audio); refresh device lists every 5 s; every 10 s (every tick while the service reports `busy`, and on `refresh_devices`) look at Equalizer APO again (`apps/clarim/src/eqapo.rs`: install check, the service's status or registry hints, which writer is available) and cache the result for the snapshot (`eq_apo`, never recomputed per snapshot); the saved output equalizers are written once at start (off the start-up path) and again whenever the last write failed, as soon as APO is installed and a writer exists. Backend impl holds `Mutex<Config>` (+ cached device lists) and saves config on every set_*. `install_eq_apo`, `set_eq_device` and `restart_audio` call the service (`eqapo::Service`, plain function pointers so tests never touch the pipe) and are described in "Equalizer APO management". `set_output_eq` persists first, then writes the file (the pipe call never runs under the config lock; writes are serialized so an older one never lands after a newer one); if the write fails the change stays saved, the error reaches the caller and `eq_apo.error`, and housekeeping retries. The saved equalizers stay in effect when the agent quits (APO applies the file); only `Neutro` (all-zero entries are skipped by the renderer) or `clarim uninstall` takes them out. Backend validation: mic numbers are clamped (NaN/inf refused); the three mic tone sliders are clamped to ±12 dB like the other numbers; `voice_level_db` is only ever written by a calibration (a stale value posted by the dashboard is ignored); `set_output_eq` clamps the gains (NaN/inf refused), refuses a `device_id` for which `endpoint_guid` is `None` and a new key once `output_eq` holds 64 entries (existing keys stay editable, removal always works); exe names are trimmed/lowercased and refused if empty, longer than 260, or containing a path separator, a control character or any of `<>:"|?*`; a new app rule is refused once `app_rules` holds 512 entries (existing keys stay editable); `AppSession.in_call` is passed through from routing to the snapshot untouched; channel `device_id` is not checked against the current device list (an unplugged device must survive other edits).
**Call tick** (housekeeping hook: `backend.rs::tick_mic`, every 1 s after the other housekeeping and still holding `mic`; the pure policy is in `apps/clarim/src/callwatch.rs`, unit-tested with a fake clock; lock order mic → cfg → leaf locks `devices`, `routing`, `call`; nothing in it joins a thread or blocks for long). Full design in `docs/leak-agc.md`.
1. **Call state.** `CallTracker` counts the Active capture sessions from `routing.captures()` whose `device_id` equals (case-insensitive) the virtual mic's capture endpoint (`virtual_mic_capture(inputs, output_name)`: the virtual capture endpoint with the same device group as the render device Clarim writes into) and whose exe is not `obs64.exe`/`obs32.exe`. Unknown exes count for `in_call` but stay out of `apps`. 8 s hangover; no cable id means never in a call.
2. **References.** While in a call, with `leak_warnings` on and the mic running, `MicEngine::set_references` gets the render devices the four channels play on (None = default) plus the default console and communications outputs, minus Clarim's own output and monitor devices, virtual devices and anything not listed, capped at 4; otherwise an empty list (the loopbacks stop).
3. **Leak alert.** `LeakMonitor` ticks on `echo_score`, `echo_db`, `echo_lag_ms`, `noise_db` and `references`. Echo raises after 4 consecutive ticks with `references > 0`, score ≥ 0.5, `echo_db` ≥ −50 and a lag within 20 ms of the previous tick; it clears after 10 ticks with score < 0.3, `echo_db` None or < −56. Noise raises after 8 ticks with `noise_db` ≥ −48 and clears after 10 ticks under −54 (or None). Echo outranks Noise. Not in a call, or `leak_warnings` off, resets everything. While in a call one log line every 10 s (`score lag echo_db voice_db agc_gain noise refs`) feeds threshold tuning from `clarim.log`.
4. **Balloon and tooltip.** `tray::notify(title, text, Balloon::Warning)` ("Som vazando na chamada": "O som das caixas está entrando no seu microfone. Abaixe o volume ou use fone." / "Ruído passando na chamada": "Tem ruído de fundo passando pelo filtro. Aproxime o microfone ou ligue o Foco na voz."): `NIF_INFO` with `NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME`, icon id 1 (tray-icon numbers its icons from 1 and Clarim builds exactly one). It returns false, so the monitor retries on the next tick, unless `SHQueryUserNotificationState` is `QUNS_ACCEPTS_NOTIFICATIONS` (so nothing is shown full screen or in a presentation; the balloon appears after alt-tab). One balloon per kind per call; the same kind again only after 10 min of continuous activity. `tray::set_tip`: "Clarim · som vazando na chamada" while an alert is active, else "Clarim · em chamada" in a call, else "Clarim"; changed only on transitions.
5. **Windows mic volume** (`MicVolumeGuard`, through `clarim-audio`'s `endpoint.rs`: `IAudioEndpointVolume` in dB, tagged with Clarim's event-context GUID, called from this thread only). Active = in a call && `auto_level` && `auto_mic_volume` && mic running && the stored record's device (if any) is the current input. It lowers by 3 dB when the clip frames of the last 2 ticks sum to 5 or more, less than 12 dB is taken and 10 s have passed since the last action. If the clip frames in the 10 ticks after a lowering are not at most half of those in the 10 ticks before it, it gives that episode back once and stays off for the run (`Ineffective`). It gives back min(3, taken) dB (never above `baseline_db`) after 10 consecutive ticks with the AGC at its maximum gain (≥ 20 − 0.5 dB) and `input_voice_db + agc_gain_db` more than 6 dB under −23 dBFS, no clip in the last 60 s and 10 s since the last action; the same quiet condition with nothing taken sets `mic_too_quiet`. A mic without hardware volume (`QueryHardwareSupport`) or with less than 3 dB of range disables the guard for the run (`NoHardware`) and sets the PT-BR note "O microfone está saturando antes do volume do Windows: diminua o 'Aumento do microfone' (Propriedades do microfone > Níveis) ou afaste o microfone." (kept for the run). After each change the read-back delta goes to `MicEngine::shift_mic_volume` (the AGC estimate follows), is added to `voice_level_db` when calibrated (Voice Focus stays valid), `mic_volume_taken` is updated or cleared, config is saved and the engine re-applied. Notes (60 s): "Baixei o volume do microfone no Windows em 3 dB porque estava estourando." / "Devolvi 3 dB ao volume do microfone no Windows (a voz estava baixa)."; one `Balloon::Info` ("Microfone estourando": "Baixei o volume do microfone no Windows em 3 dB.") per call for a lowering. Any error: warn log and the guard is off for the run. On a graceful quit (`shutdown()`) a record for a present device whose volume is still within 0.5 dB of `baseline − taken` is given back (volume set to `baseline_db`); otherwise (the user changed it meanwhile) the record is only cleared.
6. **Overlay.** `in_call`, `call_apps`, `leak`, `mic_volume_note` and `mic_too_quiet` live in a leaf `Mutex<CallOverlay>`, copied into every `MicStatus` that `mic_status()` and `snapshot()` return (also the cached `last_mic` path).

Release build: `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`, AttachConsole(ATTACH_PARENT_PROCESS) for CLI subcommands. Logs: `%LOCALAPPDATA%\Clarim\clarim.log` (truncate at 5 MB); `install` / `uninstall` log to `<Program Files>\Clarim\install.log` (truncate at 1 MB).

## Build hardening

- `.cargo/config.toml` links the CRT statically (`+crt-static`): the single installed `clarim.exe` has no `vcruntime140.dll` dependency.
- `apps/clarim/build.rs` passes `/DEPENDENTLOADFLAG:0x800` (static imports load from System32 only, no DLL planting next to the exe).
- `clarim-audio` pins `tract-{core,onnx,pulse,hir} = "=0.21.4"` as optional deps of the `dfn` feature, so a lock rebuild or `cargo update` cannot move tract (newer 0.21.x breaks libDF).

## Quality bar

- `cargo build --release` clean, `cargo clippy --workspace` no warnings in our code, `cargo test --workspace` green.
- No `unwrap()`/`expect()` on runtime paths that can fail (device, COM, IO, JSON). Every `unsafe` block has a `// SAFETY:` comment.
- Idle CPU of the agent with mic on (RNNoise) < 2% of one core; DFN mode < 8%. Last measured (release, Core i5-10400F, Windows 11, `agent --no-routing`, cycle-based, dashboard closed): Off 1.97 %, Light 1.91 %, Strong 16.5 % (DSP thread 15.3 %, ~1.45 ms per 10 ms frame in live use, 0.9 ms back-to-back in `selftest`): the Strong target is NOT met. Routing worker ≈ 0.45 % at a 2 s scan (≈ 0.9 % expected at the 1 s scan of an enabled engine, not measured live because that writes to Windows). Estimated mic-path latency: 17.6 / 27.6 / 49.0 ms (Off / Light / Strong).
- Don't present partial behaviour as working: if routing API is unavailable or no virtual mic exists, the dashboard says so.
