//! Output equalizer, agent side: finds out what Equalizer APO looks like on this PC, writes its config and
//! asks the Clarim service to install / attach / restart it (SPEC "Output equalizer (Equalizer APO)").
//!
//! Detection is read-only and only a hint: it never blocks a write. With the service answering, the service's
//! status (it reads the registry as SYSTEM and can see `audiodg.exe`) is what the dashboard gets; without it
//! the agent reads what a normal user can. The text of the file comes only from `clarim_core::render_eqapo_file`.
//! Program Files is admin-only, so the normal path is the Clarim service (named pipe); a config folder the
//! user can write is used directly (config only: install, attach and restart always need the service).

use std::collections::BTreeMap;
use std::ffi::{c_void, OsString};
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{anyhow, bail, Context, Result};
use clarim_audio::devices::is_denied;
use clarim_core::{
    endpoint_guid, is_stock_apo_config, is_virtual_device, render_eqapo_file, ChannelId, Config, DeviceInfo, EqApoStatus, OutputEq,
    EQAPO_INCLUDE_LINE,
};
use clarim_svc::ApoServiceStatus;
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HANDLE, WIN32_ERROR};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

/// The one file Clarim owns in Equalizer APO's config folder, and the line in `config.txt` that pulls it in.
const EQ_FILE: &str = "clarim-eq.txt";
const INCLUDE_LINE: &str = EQAPO_INCLUDE_LINE;

/// Equalizer APO's pre-mix and post-mix sAPO class ids (upper case): the installer / Clarim write one or both
/// under an endpoint's `FxProperties`, in whichever effect slot the install mode uses.
const APO_CLSIDS: [&str; 2] = ["{EACD2258-FCAC-4FF4-B36D-419E924A6D79}", "{EC1CC9CE-FAED-4822-828A-82A81A6F018F}"];
const MMDEVICES_RENDER: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
/// PKEY_AudioEndpoint_Disable_SysFx: DWORD 1 = Windows "audio enhancements" off. The Windows UI keeps it under the
/// endpoint's `Properties`; Equalizer APO's DeviceSelector reads and deletes it under `FxProperties`: look at both.
const DISABLE_SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";

const MSG_NO_WRITER: &str = "Instale o serviço do Clarim (clarim install) para usar o equalizador de saída";
const MSG_NOT_INSTALLED: &str = "O Equalizer APO não está instalado.";
pub const MSG_DRY_RUN: &str = "Modo de teste (--dry-run): o equalizador de saída fica desligado.";
pub const MSG_NO_SERVICE: &str = "Instale o serviço do Clarim (clarim install, como administrador) para o Clarim instalar e configurar o equalizador.";
const MSG_SERVICE_SILENT: &str = "O serviço do Clarim não respondeu sobre o equalizador. Se ele for de uma versão anterior, rode clarim.exe install de novo (como administrador) para atualizá-lo.";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------- folders

/// `<Program Files>\EqualizerAPO`, with Program Files asked of the shell and not of an environment variable.
fn apo_dir() -> Option<PathBuf> {
    // SAFETY: the id is a valid GUID; KF_FLAG_DEFAULT with a null token asks for the system-wide folder.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles as *const GUID, KF_FLAG_DEFAULT, HANDLE::default()) }.ok()?;
    // SAFETY: on success `path` is a NUL-terminated UTF-16 string the shell allocated. It is copied out
    // first, then freed exactly once with CoTaskMemFree, as SHGetKnownFolderPath requires.
    let folder = unsafe {
        let folder = PathBuf::from(OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0.cast::<c_void>().cast_const()));
        folder
    };
    folder.is_absolute().then(|| folder.join("EqualizerAPO"))
}

fn installed() -> bool {
    apo_dir().is_some_and(|d| d.join("EqualizerAPO.dll").is_file())
}

fn config_dir() -> Option<PathBuf> {
    apo_dir().map(|d| d.join("config"))
}

/// Can this user create files in `dir`? Probes with a temp file; never creates the folder.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!("clarim-probe-{}.tmp", std::process::id()));
    let ok = std::fs::File::create(&probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

// ---------------------------------------------------------------- registry hints

/// Read-only registry key under HKLM, closed on drop.
struct Key(HKEY);

impl Key {
    fn open(path: &str) -> std::result::Result<Key, WIN32_ERROR> {
        let path = wide(path);
        let mut key = HKEY::default();
        // SAFETY: `path` is NUL-terminated and outlives the call; `key` is a valid out pointer.
        let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr()), 0, KEY_READ, &mut key) };
        if status == ERROR_SUCCESS {
            Ok(Key(key))
        } else {
            Err(status)
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful RegOpenKeyExW and is closed exactly once, here.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Does this registry value data (any type, read as UTF-16) hold an Equalizer APO class id?
fn has_apo_clsid(data: &[u8]) -> bool {
    let units = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    let text: String = char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect::<String>().to_ascii_uppercase();
    APO_CLSIDS.iter().any(|id| text.contains(id))
}

/// `Some(true)` when a value under the endpoint's `FxProperties` holds the APO class id, `Some(false)`
/// when none does (or the key does not exist), `None` when the key cannot be read.
fn fx_has_apo(guid: &str) -> Option<bool> {
    let key = match Key::open(&format!(r"{MMDEVICES_RENDER}\{guid}\FxProperties")) {
        Ok(key) => key,
        Err(e) if e == ERROR_FILE_NOT_FOUND => return Some(false),
        Err(_) => return None,
    };
    for index in 0..1024 {
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut data = [0u8; 2048];
        let mut data_len = data.len() as u32;
        // SAFETY: `key` is open; both buffers are valid for the lengths passed in, and the two in/out
        // lengths are valid pointers.
        let status = unsafe {
            RegEnumValueW(key.0, index, PWSTR(name.as_mut_ptr()), &mut name_len, None, None, Some(data.as_mut_ptr()), Some(&mut data_len))
        };
        match status {
            ERROR_SUCCESS if has_apo_clsid(&data[..(data_len as usize).min(data.len())]) => return Some(true),
            ERROR_SUCCESS => {}
            ERROR_NO_MORE_ITEMS => return Some(false),
            ERROR_MORE_DATA => {} // too big to be a class id string: skip it
            _ => return None,
        }
    }
    Some(false)
}

/// Windows "audio enhancements" switched off for this endpoint: no APO runs there (`Properties` or `FxProperties`).
fn enhancements_off(guid: &str) -> bool {
    ["Properties", "FxProperties"].iter().any(|sub| {
        let key = wide(&format!(r"{MMDEVICES_RENDER}\{guid}\{sub}"));
        let name = wide(DISABLE_SYSFX);
        let (mut value, mut bytes) = (0u32, 4u32);
        // SAFETY: both strings are NUL-terminated and outlive the call; `value` is valid for `bytes` bytes
        // and `bytes` is a valid in/out pointer.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut value as *mut u32).cast::<c_void>()),
                Some(&mut bytes),
            )
        };
        status == ERROR_SUCCESS && value == 1
    })
}

// ---------------------------------------------------------------- detection

/// Last writability probe (`None` = never run) and whether APO was installed at that time.
/// The probe creates a file in APO's config folder, which makes APO reload its config (a possible audio
/// glitch), so it runs once and on demand, not on every periodic `detect`.
static PROBE: Mutex<(Option<bool>, bool)> = Mutex::new((None, false));

/// Probe the config folder again? Only when never probed, when asked (`force`), or when APO has just appeared.
fn should_probe(cached: Option<bool>, force: bool, installed_changed: bool) -> bool {
    cached.is_none() || force || installed_changed
}

/// Can this user write APO's config folder? The cached answer, unless `should_probe` says to look again.
fn writable_cached(installed: bool, force: bool) -> bool {
    let mut probe = PROBE.lock().unwrap_or_else(PoisonError::into_inner);
    let (cached, was_installed) = *probe;
    let writable = match cached {
        Some(w) if !should_probe(cached, force, installed && !was_installed) => w,
        _ => config_dir().is_some_and(|d| dir_writable(&d)),
    };
    *probe = (Some(writable), installed);
    writable
}

/// The Clarim service as the agent uses it, as plain function pointers so tests never touch the pipe or the
/// machine. `Service::REAL` is the production wiring.
#[derive(Clone, Copy)]
pub struct Service {
    pub available: fn() -> bool,
    pub status: fn() -> Result<ApoServiceStatus>,
    pub install: fn(&[String]) -> Result<()>,
    pub set_device: fn(&str, bool) -> Result<()>,
    pub restart: fn() -> Result<()>,
}

impl Service {
    pub const REAL: Service = Service {
        available: clarim_svc::eqapo_service_available,
        status: clarim_svc::apo_status,
        install: clarim_svc::apo_install,
        set_device: clarim_svc::apo_set_device,
        restart: clarim_svc::apo_restart_audio,
    };
}

/// Who could write the APO config right now, plus the PT-BR reason when nobody can. The service never
/// needs the probe.
fn pick_writer(dry_run: bool, installed: bool, force: bool, svc: &Service) -> (&'static str, Option<String>) {
    if dry_run {
        ("none", Some(MSG_DRY_RUN.to_string()))
    } else if (svc.available)() {
        ("service", None)
    } else if writable_cached(installed, force) {
        ("direct", None)
    } else {
        // without APO the service is also what would install it, so that is the reason worth giving
        ("none", Some(if installed { MSG_NO_WRITER } else { MSG_NO_SERVICE }.to_string()))
    }
}

/// Read-only look at Equalizer APO: installed or not, which of `outputs` it seems to be set up on, how the
/// config could be written, and (with the Clarim service answering) what the service says: a running install or
/// restart, whether Windows Audio must restart, and per-device state. Cheap (a few registry reads and one pipe
/// call), but not free: callers cache it. `dry_run` = never claim a writer and never call the service, so the
/// dashboard offers nothing that would write. `force` = probe the config folder's writability again instead of
/// reusing the last answer.
pub fn detect(outputs: &[DeviceInfo], dry_run: bool, force: bool, svc: &Service) -> EqApoStatus {
    let mut st = EqApoStatus { installed: installed(), ..EqApoStatus::default() };
    if st.installed {
        for d in outputs {
            let Some(guid) = endpoint_guid(&d.id) else { continue };
            match fx_has_apo(guid) {
                Some(true) => st.active_devices.push(d.id.clone()),
                Some(false) => {}
                None => st.unknown_devices.push(d.id.clone()),
            }
            if enhancements_off(guid) {
                st.enhancements_off.push(d.id.clone());
            }
        }
    }
    let (writer, error) = pick_writer(dry_run, st.installed, force, svc);
    st.writer = writer.to_string();
    st.error = error;
    if writer == "service" {
        match (svc.status)() {
            Ok(from_service) => fold_service(&mut st, outputs, &from_service),
            Err(e) => {
                log::debug!("equalizer: no status from the service: {e:#}");
                st.error = Some(MSG_SERVICE_SILENT.to_string());
            }
        }
    }
    st
}

/// What the service reports replaces the agent's own registry hints (the service reads as SYSTEM, includes
/// installs made by DeviceSelector, and checks both places of the "enhancements off" flag). A device the
/// service does not list (not present, or past its 100-device cap) is "unknown": the dashboard claims nothing.
///
/// `apo_loaded` is passed through as information only. `Some(false)` is normal while nothing plays on a
/// configured device (Clarim's own mic engine keeps `audiodg.exe` alive by rendering into the cable), so it
/// never decides anything; `needs_restart` (registry change after `audiodg.exe` started) does.
fn fold_service(st: &mut EqApoStatus, outputs: &[DeviceInfo], svc: &ApoServiceStatus) {
    st.installed = svc.installed;
    st.busy = svc.busy.clone();
    st.needs_restart = svc.needs_restart;
    st.apo_loaded = svc.apo_loaded;
    st.devices = svc.devices.clone();
    st.error = svc.error.clone();
    st.active_devices.clear();
    st.unknown_devices.clear();
    st.enhancements_off.clear();
    for d in outputs {
        let Some(guid) = endpoint_guid(&d.id) else { continue };
        match svc.devices.iter().find(|s| s.guid.eq_ignore_ascii_case(guid)) {
            Some(s) => {
                if svc.installed && s.configured {
                    st.active_devices.push(d.id.clone());
                }
                if svc.installed && s.enhancements_off {
                    st.enhancements_off.push(d.id.clone());
                }
            }
            None => st.unknown_devices.push(d.id.clone()),
        }
    }
}

// ---------------------------------------------------------------- which devices

/// Clarim never attaches Equalizer APO to a virtual device (the mic's cable, Sonar, Voicemeeter...) or a device on
/// the deny list. The service refuses virtual ones too; this keeps them out of the request.
pub fn eq_capable(dev: &DeviceInfo, deny: &[String]) -> bool {
    let name = dev.name.to_lowercase();
    !dev.is_virtual
        && !is_virtual_device(&dev.name)
        && !["vb-audio", "voicemeeter", "cable"].iter().any(|m| name.contains(m))
        && !is_denied(&dev.name, deny)
}

/// Endpoint GUIDs "Instalar equalizador" attaches Equalizer APO to: the distinct physical devices the four
/// channels play on, in channel order. A channel on the Windows default (or on a device that is unplugged) plays
/// on the default device. `outputs` is the full device list, not the deny-filtered one.
pub fn install_targets(cfg: &Config, outputs: &[DeviceInfo]) -> Vec<String> {
    let default = outputs.iter().find(|d| d.is_default);
    let mut guids: Vec<String> = Vec::new();
    for ch in ChannelId::ALL {
        let own = cfg.channels.get(&ch).and_then(|c| c.device_id.as_deref()).and_then(|id| outputs.iter().find(|d| d.id == id));
        let Some(dev) = own.or(default) else { continue };
        let Some(guid) = endpoint_guid(&dev.id) else { continue };
        if eq_capable(dev, &cfg.deny_devices) && !guids.iter().any(|g| g.eq_ignore_ascii_case(guid)) {
            guids.push(guid.to_string());
        }
    }
    guids
}

// ---------------------------------------------------------------- writing

/// `(GUID, EQ)` pairs for the renderer. A key that is not an endpoint id (a hand-edited config) is left
/// out instead of failing the whole file.
fn entries(map: &BTreeMap<String, OutputEq>) -> Vec<(String, OutputEq)> {
    map.iter().filter_map(|(id, eq)| endpoint_guid(id).map(|g| (g.to_string(), *eq))).collect()
}

/// Writes every saved output EQ to Equalizer APO: through the Clarim service when it runs, else straight
/// into the config folder when this user may write there. Errors are PT-BR, ready for the dashboard.
pub fn apply(output_eq: &BTreeMap<String, OutputEq>) -> Result<()> {
    let entries = entries(output_eq);
    if !installed() {
        // nothing to write and no APO to write it for (e.g. the last EQ removed after an uninstall) is fine
        if entries.iter().all(|(_, eq)| *eq == OutputEq::default()) {
            return Ok(());
        }
        bail!(MSG_NOT_INSTALLED);
    }
    if clarim_svc::eqapo_service_available() {
        return clarim_svc::eqapo_send(&entries).context("O serviço do Clarim não conseguiu gravar o equalizador");
    }
    let dir = config_dir().filter(|d| dir_writable(d)).ok_or_else(|| anyhow!(MSG_NO_WRITER))?;
    write_in(&dir, &entries)
}

/// The direct path: `clarim-eq.txt` (tmp + rename) and the `Include:` line in `config.txt`.
// ponytail: same job as the service's writer; duplicated here because clarim-svc keeps its helper private.
fn write_in(dir: &Path, entries: &[(String, OutputEq)]) -> Result<()> {
    let text = render_eqapo_file(entries)?;
    let (tmp, target) = (dir.join(format!("{EQ_FILE}.tmp")), dir.join(EQ_FILE));
    std::fs::write(&tmp, text).with_context(|| format!("Não foi possível gravar {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("Não foi possível gravar {}", target.display()));
    }
    ensure_include(&dir.join("config.txt"))
}

/// Is our `Include:` line missing from this `config.txt` text?
fn include_missing(config: &str) -> bool {
    !config.lines().any(|l| l.trim_start_matches('\u{feff}').trim().eq_ignore_ascii_case(INCLUDE_LINE))
}

/// Appends `Include: clarim-eq.txt` to `config.txt` once (leading newline), after backing the original up
/// once as `config.txt.clarim-backup`. No other line is touched; a file that is not UTF-8 is left alone.
/// Exception: Equalizer APO's own stock `config.txt` (`is_stock_apo_config`) lowers every device by 6 dB, so it
/// becomes just the Include line (the backup keeps the stock file).
fn ensure_include(config: &Path) -> Result<()> {
    let original = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("Não foi possível ler {}", config.display())),
    };
    let stock = is_stock_apo_config(&original);
    if !stock && !include_missing(&original) {
        return Ok(());
    }
    let backup = config.with_file_name("config.txt.clarim-backup");
    if config.is_file() && !backup.exists() {
        std::fs::copy(config, &backup).with_context(|| format!("Não foi possível fazer o backup de {}", config.display()))?;
    }
    if stock {
        return std::fs::write(config, format!("{INCLUDE_LINE}\n"))
            .with_context(|| format!("Não foi possível alterar {}", config.display()))
            .or_else(|e| {
                if include_missing(&original) {
                    return Err(e);
                }
                // stock + our line already works: a file that cannot be rewritten (read-only) only keeps its -6 dB
                log::warn!("could not neutralize Equalizer APO's stock config.txt: {e:#}");
                Ok(())
            });
    }
    let mut file = std::fs::OpenOptions::new().append(true).create(true).open(config)
        .with_context(|| format!("Não foi possível alterar {}", config.display()))?;
    file.write_all(format!("\n{INCLUDE_LINE}\n").as_bytes())
        .with_context(|| format!("Não foi possível alterar {}", config.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUID: &str = "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}";

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn apo_class_id_is_found_in_registry_data_whatever_the_case() {
        assert!(has_apo_clsid(&utf16("{ec1cc9ce-faed-4822-828a-82a81a6f018f}")));
        assert!(has_apo_clsid(&utf16("{eacd2258-fcac-4ff4-b36d-419e924a6d79}")), "the pre-mix class counts too");
        assert!(has_apo_clsid(&utf16("{D04E05A6-594B-4FB6-A80D-01AF5EED7D1D},2 {EC1CC9CE-FAED-4822-828A-82A81A6F018F}")));
        assert!(!has_apo_clsid(&utf16("{00000000-0000-0000-0000-000000000000}")));
        assert!(!has_apo_clsid(&[]) && !has_apo_clsid(&[0xEC]));
    }

    #[test]
    fn only_endpoint_ids_reach_the_renderer() {
        let eq = OutputEq { bass_db: 5.0, ..OutputEq::default() };
        let map = BTreeMap::from([
            (format!("{{0.0.0.00000000}}.{GUID}"), eq),
            ("out-rtk".to_string(), eq),
            (format!("{{0.0.0.00000000}}.{GUID}\nInclude: x"), eq),
        ]);
        assert_eq!(entries(&map), vec![(GUID.to_string(), eq)]);
    }

    /// Read-only: an endpoint that does not exist has no APO and is not "unreadable"; an id without a GUID is skipped.
    #[test]
    fn registry_hints_for_a_missing_endpoint() {
        let nowhere = "{00000000-0000-0000-0000-00000000dead}";
        assert_eq!(fx_has_apo(nowhere), Some(false));
        assert!(!enhancements_off(nowhere));
    }

    #[test]
    fn the_include_line_is_appended_once_and_the_original_is_backed_up_once() {
        let dir = std::env::temp_dir().join(format!("clarim-eqapo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.txt");
        let original = "Preamp: -3 dB\r\nFilter: ON PK Fc 100 Hz Gain 2 dB Q 1\r\n";
        std::fs::write(&config, original).expect("seed config.txt");
        let eq = OutputEq { bass_db: 5.0, mid_db: 0.0, treble_db: -2.0 };
        let entries = vec![(GUID.to_string(), eq)];

        write_in(&dir, &entries).expect("first write");
        let after = std::fs::read_to_string(&config).expect("config.txt");
        assert_eq!(after, format!("{original}\n{INCLUDE_LINE}\n"));
        assert_eq!(std::fs::read_to_string(dir.join("config.txt.clarim-backup")).expect("backup"), original);
        assert_eq!(std::fs::read_to_string(dir.join(EQ_FILE)).expect("clarim-eq.txt"), render_eqapo_file(&entries).expect("render"));

        // again, with a different EQ: the Include is not repeated and the backup keeps the true original
        write_in(&dir, &[(GUID.to_string(), OutputEq::default())]).expect("second write");
        assert_eq!(std::fs::read_to_string(&config).expect("config.txt"), after);
        assert_eq!(std::fs::read_to_string(dir.join("config.txt.clarim-backup")).expect("backup"), original);
        assert_eq!(std::fs::read_to_string(dir.join(EQ_FILE)).expect("clarim-eq.txt"), "# Clarim output equalizer - generated, do not edit\nDevice: all\n");
        assert!(!dir.join(format!("{EQ_FILE}.tmp")).exists(), "the temp file is renamed away");
        assert!(include_missing("# Include: clarim-eq.txt\n") && !include_missing("a\n  include: CLARIM-EQ.txt  \n"));
        assert!(!include_missing("\u{feff}Include: clarim-eq.txt\r\n"), "a UTF-8 BOM before the first line is ignored");

        // a bad id fails the whole write and leaves the previous file alone
        assert!(write_in(&dir, &[("{x}".to_string(), eq)]).is_err());
        assert!(std::fs::read_to_string(dir.join(EQ_FILE)).expect("clarim-eq.txt").ends_with("Device: all\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stock_apo_config_becomes_just_the_include_line_and_a_customised_one_only_gets_the_append() {
        let stock = "Preamp: -6 dB\r\nInclude: example.txt\r\nGraphicEQ: 25 0; 40 0; 63 0; 100 0; 160 0; 250 0; 400 0; 630 0; 1000 0; 1600 0; 2500 0; 4000 0; 6300 0; 10000 0; 16000 0";
        let entries = vec![(GUID.to_string(), OutputEq { bass_db: 5.0, ..OutputEq::default() })];
        let run = |tag: &str, seed: &str| {
            let dir = std::env::temp_dir().join(format!("clarim-eqapo-stock-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            std::fs::write(dir.join("config.txt"), seed).expect("seed config.txt");
            write_in(&dir, &entries).expect("write");
            let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
            let out = (read("config.txt"), read("config.txt.clarim-backup"));
            let _ = std::fs::remove_dir_all(&dir);
            out
        };
        assert_eq!(run("stock", stock), (format!("{INCLUDE_LINE}\n"), stock.to_string()), "no -6 dB; the backup is the stock file");
        let appended = format!("{stock}\n{INCLUDE_LINE}\n"); // an older Clarim had appended its line
        assert_eq!(run("appended", &appended), (format!("{INCLUDE_LINE}\n"), appended.clone()));
        let custom = stock.replace("-6 dB", "-5 dB");
        assert_eq!(run("custom", &custom), (format!("{custom}\n{INCLUDE_LINE}\n"), custom.clone()), "customised: only the append");
    }

    #[test]
    fn a_read_only_stock_config_that_has_our_line_does_not_fail_the_write() {
        let stock = "Preamp: -6 dB\r\nInclude: example.txt\r\nGraphicEQ: 25 0; 40 0; 63 0; 100 0; 160 0; 250 0; 400 0; 630 0; 1000 0; 1600 0; 2500 0; 4000 0; 6300 0; 10000 0; 16000 0";
        let entries = vec![(GUID.to_string(), OutputEq { bass_db: 5.0, ..OutputEq::default() })];
        let dir = std::env::temp_dir().join(format!("clarim-eqapo-stock-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let appended = format!("{stock}\n{INCLUDE_LINE}\n");
        let (with_line, without_line) = (dir.join("with-line"), dir.join("without-line"));
        let set_read_only = |dir: &Path, on: bool| {
            let mut perms = std::fs::metadata(dir.join("config.txt")).unwrap().permissions();
            perms.set_readonly(on);
            std::fs::set_permissions(dir.join("config.txt"), perms).unwrap();
        };
        let run = |dir: &Path, seed: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("config.txt"), seed).unwrap();
            set_read_only(dir, true);
            let written = write_in(dir, &entries);
            set_read_only(dir, false);
            (written.is_ok(), std::fs::read_to_string(dir.join("config.txt")).unwrap())
        };
        assert_eq!(run(&with_line, &appended), (true, appended.clone()), "the Include works already: not a failed write");
        assert_eq!(run(&without_line, stock), (false, stock.to_string()), "the write is needed here, so it fails");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read-only: prints what the registry hints say for this PC's playback devices (a `None` means the key
    /// could not be read): `cargo test -p clarim live_registry_hints -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_registry_hints() {
        let outputs = crate::backend::scan_devices().expect("device scan").outputs;
        for d in &outputs {
            let Some(guid) = endpoint_guid(&d.id) else { continue };
            println!("{:<60} {guid} apo={:?} enhancements_off={}", d.name, fx_has_apo(guid), enhancements_off(guid));
        }
        println!("{:#?}", detect(&outputs, true, false, &Service::REAL));
    }

    // ---- the service's status, and which devices get the equalizer

    const RTK: &str = "{171dffe0-1f81-4558-a253-45945204afa2}";
    const USB: &str = "{9fc5e635-237f-4f55-add4-030c65c6a5c9}";
    const BT: &str = "{b9cc1e4e-11bd-4f54-8227-5e28f30a485d}";
    const CABLE: &str = "{aee288e6-0000-4000-8000-000000000001}";
    const HDMI: &str = "{11111111-0000-4000-8000-000000000002}";

    fn out(guid: &str, name: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo { id: format!("{{0.0.0.00000000}}.{guid}"), name: name.into(), is_default, is_virtual: is_virtual_device(name) }
    }

    fn id(guid: &str) -> String {
        format!("{{0.0.0.00000000}}.{guid}")
    }

    fn pc() -> Vec<DeviceInfo> {
        vec![
            out(RTK, "Alto-falantes (Realtek(R) Audio)", true),
            out(USB, "Alto-falantes (7- USB Audio Device)", false),
            out(BT, "Fones de ouvido (WZ-BT5.0)", false),
            out(CABLE, "CABLE Input (VB-Audio Virtual Cable)", false),
            out(HDMI, "Speakers (NVIDIA High Definition Audio)", false),
        ]
    }

    fn on(cfg: &mut Config, ch: ChannelId, guid: &str) {
        cfg.channels.entry(ch).or_default().device_id = Some(id(guid));
    }

    #[test]
    fn install_targets_are_the_distinct_physical_devices_of_the_channels() {
        let outs = pc();
        let mut cfg = Config::default();
        // every channel on the Windows default: that one device, once
        assert_eq!(install_targets(&cfg, &outs), [RTK]);
        // Game follows the default, Chat is on Bluetooth, Media on USB, Aux on the virtual cable (never)
        on(&mut cfg, ChannelId::Chat, BT);
        on(&mut cfg, ChannelId::Media, USB);
        on(&mut cfg, ChannelId::Aux, CABLE);
        assert_eq!(install_targets(&cfg, &outs), [RTK, BT, USB], "channel order, no duplicates, no cable");
        // a device that is unplugged plays on the default; two channels on one device count once
        on(&mut cfg, ChannelId::Media, "{deadbeef-0000-4000-8000-000000000003}");
        on(&mut cfg, ChannelId::Game, BT);
        assert_eq!(install_targets(&cfg, &outs), [BT, RTK]);
        // the deny list keeps a device out (HDMI of the graphics card)
        on(&mut cfg, ChannelId::Aux, HDMI);
        assert_eq!(install_targets(&cfg, &outs), [BT, RTK]);
        // a virtual Windows default and nothing else physical: nothing to attach (APO alone gets installed)
        let only_virtual = [
            out(CABLE, "CABLE Input (VB-Audio Virtual Cable)", true),
            out("{22222222-0000-4000-8000-000000000004}", "Voicemeeter Input (VB-Audio Voicemeeter VAIO)", false),
        ];
        assert!(install_targets(&Config::default(), &only_virtual).is_empty());
        assert!(install_targets(&Config::default(), &[]).is_empty());
    }

    #[test]
    fn only_physical_devices_off_the_deny_list_can_take_the_equalizer() {
        let deny = Config::default().deny_devices;
        let ok: Vec<bool> = pc().iter().map(|d| eq_capable(d, &deny)).collect();
        assert_eq!(ok, [true, true, true, false, false]);
        let odd = |name: &str| DeviceInfo { id: id(RTK), name: name.into(), is_default: false, is_virtual: false };
        for name in ["Voicemeeter Out B1", "VB-Audio Point", "SteelSeries Sonar - Gaming", "Mixagem estéreo", "Steam Streaming Speakers"] {
            assert!(!eq_capable(&odd(name), &[]), "{name}");
        }
        assert!(!eq_capable(&DeviceInfo { is_virtual: true, ..odd("Headset") }, &[]), "the flag alone is enough");
    }

    fn svc_status() -> ApoServiceStatus {
        let dev = |guid: &str, configured: bool, enhancements_off: bool| clarim_core::EqApoDevice {
            guid: guid.into(),
            configured,
            can_configure: true,
            enhancements_off,
        };
        ApoServiceStatus {
            installed: true,
            busy: Some("Reiniciando o áudio do Windows...".into()),
            error: Some("O áudio voltou, mas estes serviços não: X.".into()),
            needs_restart: true,
            apo_loaded: Some(false),
            // upper-case GUID in the service's list: ids from the audio API are lower case, the match must not care
            devices: vec![dev(&RTK.to_uppercase(), true, false), dev(BT, true, true), dev(USB, false, false)],
        }
    }

    #[test]
    fn the_service_status_replaces_the_agents_own_hints_and_maps_guids_to_endpoint_ids() {
        let outs = pc();
        let mut st = EqApoStatus { writer: "service".into(), active_devices: vec!["stale".into()], ..EqApoStatus::default() };
        fold_service(&mut st, &outs, &svc_status());
        assert!(st.installed && st.needs_restart);
        assert_eq!(st.busy.as_deref(), Some("Reiniciando o áudio do Windows..."));
        assert_eq!(st.error.as_deref(), Some("O áudio voltou, mas estes serviços não: X."));
        assert_eq!(st.apo_loaded, Some(false));
        assert_eq!(st.active_devices, [id(RTK), id(BT)], "the stale local hint is gone");
        assert_eq!(st.enhancements_off, [id(BT)]);
        // CABLE and HDMI are not in the service's list: no claim either way
        assert_eq!(st.unknown_devices, [id(CABLE), id(HDMI)]);
        assert_eq!(st.devices.len(), 3);
        assert_eq!(st.writer, "service", "the writer is not the status' business");

        // APO not installed: a leftover registry entry is no claim
        let mut st = EqApoStatus::default();
        fold_service(&mut st, &outs, &ApoServiceStatus { installed: false, ..svc_status() });
        assert!(!st.installed && st.active_devices.is_empty() && st.enhancements_off.is_empty());
        // an id without a GUID is skipped, not unknown
        let mut st = EqApoStatus::default();
        let odd = [DeviceInfo { id: "out-rtk".into(), name: "x".into(), is_default: false, is_virtual: false }];
        fold_service(&mut st, &odd, &svc_status());
        assert!(st.unknown_devices.is_empty());
    }

    static STATUS_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A service that is there (or not) and answers `status` (or not). Plain fn pointers cannot capture, so the
    /// variants the tests need are spelled out.
    fn fake_service(available: bool, answers: bool) -> Service {
        fn yes() -> bool {
            true
        }
        fn no() -> bool {
            false
        }
        fn good() -> Result<ApoServiceStatus> {
            STATUS_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(svc_status())
        }
        fn silent() -> Result<ApoServiceStatus> {
            STATUS_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(anyhow!("Pedido inválido."))
        }
        fn never() -> Result<()> {
            Err(anyhow!("must not be called"))
        }
        Service {
            available: if available { yes } else { no },
            status: if answers { good } else { silent },
            install: |_| never(),
            set_device: |_, _| never(),
            restart: never,
        }
    }

    #[test]
    fn detect_asks_the_service_only_when_it_is_there_and_not_in_dry_run() {
        use std::sync::atomic::Ordering::SeqCst;
        let outs = pc();
        // the service answers: its status is what comes out (writer "service")
        let before = STATUS_CALLS.load(SeqCst);
        let st = detect(&outs, false, false, &fake_service(true, true));
        assert_eq!(st.writer, "service");
        assert!(st.needs_restart && st.busy.is_some() && st.installed);
        assert_eq!(STATUS_CALLS.load(SeqCst), before + 1);
        // it is there but does not understand "status" (an older service): writes still work, the error says what to do
        let st = detect(&outs, false, false, &fake_service(true, false));
        assert_eq!(st.writer, "service");
        assert!(st.error.as_deref().is_some_and(|e| e.contains("clarim.exe install")), "{:?}", st.error);
        assert!(!st.needs_restart && st.busy.is_none() && st.devices.is_empty());
        // --dry-run never calls the service
        let before = STATUS_CALLS.load(SeqCst);
        let st = detect(&outs, true, false, &fake_service(true, true));
        assert_eq!((st.writer.as_str(), st.error.as_deref()), ("none", Some(MSG_DRY_RUN)));
        assert_eq!(STATUS_CALLS.load(SeqCst), before);
        // (no service: `pick_writer` would probe the real config folder for a writable temp file, which makes
        // Equalizer APO reload; that path is covered by the probe-cache tests below instead)
    }

    #[test]
    fn the_folder_probe_runs_once_then_only_when_forced_or_apo_appears() {
        assert!(should_probe(None, false, false), "never probed yet");
        assert!(!should_probe(Some(true), false, false) && !should_probe(Some(false), false, false), "periodic detect reuses it");
        assert!(should_probe(Some(false), true, false), "dashboard refresh");
        assert!(should_probe(Some(false), false, true), "APO just got installed");
    }

    #[test]
    fn a_writable_folder_is_probed_and_a_missing_one_is_not_created() {
        let dir = std::env::temp_dir().join(format!("clarim-eqapo-probe-{}", std::process::id()));
        assert!(!dir_writable(&dir) && !dir.exists());
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(dir_writable(&dir));
        assert_eq!(std::fs::read_dir(&dir).expect("read dir").count(), 0, "the probe file is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
