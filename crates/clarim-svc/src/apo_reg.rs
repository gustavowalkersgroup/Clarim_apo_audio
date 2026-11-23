//! The thin Win32 registry layer under `apo_plan`: reads one playback endpoint's state, executes a
//! `Plan`, saves a backup first. No decision is made here.
//!
//! Safety rules, all checked in `resolve_endpoint` before any key is opened for writing: the endpoint id
//! must be a braced GUID (`^\{[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}\}$`) AND an existing subkey of
//! `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render`. Nothing from a request ever
//! reaches a registry path otherwise. Keys are opened with only the rights the operation needs.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use clarim_core::is_braced_guid;

use crate::user_error::user_err;
use serde_json::json;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, FILETIME,
    WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
    RegQueryInfoKeyW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE,
    KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_VALUE_TYPE,
};

use crate::apo_plan::{tracked_fx_names, DeviceRegState, Op, Plan, RegValue, Values, COMBINED_DEVICE, SYSFX};

const RENDER: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
const CHILD_ROOT: &str = r"SOFTWARE\EqualizerAPO\Child APOs";
const AUDIO: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Audio";
const APO_ROOT: &str = r"SOFTWARE\EqualizerAPO";
/// Under an endpoint's `Properties`: `{a45c254e-...},2` = the connection ("Speakers"), `{b3f8fa53-...},6` =
/// the device ("Realtek(R) Audio"). Together they are the name Windows shows.
const PROP_CONNECTION: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
const PROP_DEVICE: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";
const DEVICE_NOT_PRESENT: u32 = 0x4;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn check(status: WIN32_ERROR, what: &str) -> Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(anyhow!("registry: {what} failed (Win32 error {})", status.0))
    }
}

fn not_found(status: WIN32_ERROR) -> bool {
    status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND
}

/// An open HKLM subkey, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful RegOpenKeyExW / RegCreateKeyExW and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl Key {
    /// `Ok(None)` when the key does not exist.
    fn open(path: &str, access: REG_SAM_FLAGS) -> Result<Option<Key>> {
        let path_w = wide(path);
        let mut handle = HKEY::default();
        // SAFETY: `path_w` is NUL-terminated and outlives the call; `handle` is a valid out pointer.
        let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(path_w.as_ptr()), 0, access, &mut handle) };
        if not_found(status) {
            return Ok(None);
        }
        check(status, &format!("open {path}"))?;
        Ok(Some(Key(handle)))
    }

    /// Opens, creating the key (and its parents) when missing.
    fn create(path: &str, access: REG_SAM_FLAGS) -> Result<Key> {
        let path_w = wide(path);
        let mut handle = HKEY::default();
        // SAFETY: `path_w` is NUL-terminated and outlives the call; the class and security attributes are
        // null, `handle` is a valid out pointer.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path_w.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                access,
                None,
                &mut handle,
                None,
            )
        };
        check(status, &format!("create {path}"))?;
        Ok(Key(handle))
    }

    /// `Ok(None)` when the value does not exist.
    fn get(&self, name: &str) -> Result<Option<RegValue>> {
        let name_w = wide(name);
        let mut len = 0u32;
        let mut ty = REG_VALUE_TYPE(0);
        // SAFETY: `name_w` is NUL-terminated; a null data pointer only asks for the size, `ty` and `len` are valid.
        let status = unsafe { RegQueryValueExW(self.0, PCWSTR(name_w.as_ptr()), None, Some(&raw mut ty), None, Some(&raw mut len)) };
        if not_found(status) {
            return Ok(None);
        }
        check(status, &format!("query {name}"))?;
        let mut buf = vec![0u8; len as usize + 2];
        for _ in 0..4 {
            let mut size = buf.len() as u32;
            // SAFETY: `buf` is valid for `size` bytes; `ty` and `size` are valid in/out pointers.
            let status = unsafe {
                RegQueryValueExW(self.0, PCWSTR(name_w.as_ptr()), None, Some(&raw mut ty), Some(buf.as_mut_ptr()), Some(&raw mut size))
            };
            if status == ERROR_MORE_DATA {
                buf = vec![0u8; size as usize + 64]; // the value grew between the two calls
                continue;
            }
            if not_found(status) {
                return Ok(None);
            }
            check(status, &format!("read {name}"))?;
            return Ok(Some(RegValue::decode(ty.0, &buf[..(size as usize).min(buf.len())])));
        }
        bail!("registry: {name} keeps changing while it is read")
    }

    fn set(&self, name: &str, value: &RegValue) -> Result<()> {
        let name_w = wide(name);
        let (ty, data) = value.encode();
        // SAFETY: `name_w` is NUL-terminated and `data` outlives the call, which copies it.
        let status = unsafe { RegSetValueExW(self.0, PCWSTR(name_w.as_ptr()), 0, REG_VALUE_TYPE(ty), Some(data.as_slice())) };
        check(status, &format!("set {name}"))
    }

    /// A value that is already gone is fine.
    fn delete_value(&self, name: &str) -> Result<()> {
        let name_w = wide(name);
        // SAFETY: `name_w` is NUL-terminated and outlives the call.
        let status = unsafe { RegDeleteValueW(self.0, PCWSTR(name_w.as_ptr())) };
        if not_found(status) {
            return Ok(());
        }
        check(status, &format!("delete value {name}"))
    }

    /// `(subkey count, value count, longest value name, longest value data, last write as FILETIME ticks)`.
    fn info(&self) -> Result<(u32, u32, u32, u32, u64)> {
        let (mut subkeys, mut values, mut name_len, mut data_len) = (0u32, 0u32, 0u32, 0u32);
        let mut written = FILETIME::default();
        // SAFETY: every out pointer is valid for the call; the class buffer is not requested.
        let status = unsafe {
            RegQueryInfoKeyW(
                self.0,
                PWSTR::null(),
                None,
                None,
                Some(&raw mut subkeys),
                None,
                None,
                Some(&raw mut values),
                Some(&raw mut name_len),
                Some(&raw mut data_len),
                None,
                Some(&raw mut written),
            )
        };
        check(status, "query key info")?;
        Ok((subkeys, values, name_len, data_len, filetime(written)))
    }

    /// Every value of the key.
    fn values(&self) -> Result<Vec<(String, RegValue)>> {
        let (_, count, max_name, max_data, _) = self.info()?;
        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count.saturating_add(8) {
            let mut name = vec![0u16; max_name as usize + 2];
            let mut name_len = name.len() as u32;
            let mut data = vec![0u8; max_data as usize + 2];
            let mut data_len = data.len() as u32;
            let mut ty = 0u32;
            // SAFETY: both buffers are valid for the lengths passed in; the in/out pointers are valid.
            let status = unsafe {
                RegEnumValueW(
                    self.0,
                    index,
                    PWSTR(name.as_mut_ptr()),
                    &mut name_len,
                    None,
                    Some(&raw mut ty),
                    Some(data.as_mut_ptr()),
                    Some(&raw mut data_len),
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            check(status, "enumerate values")?;
            let name = String::from_utf16_lossy(&name[..name_len as usize]);
            out.push((name, RegValue::decode(ty, &data[..(data_len as usize).min(data.len())])));
        }
        Ok(out)
    }

    fn subkeys(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for index in 0..4096u32 {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: `name` is valid for `len` units; the class and reserved parameters are not used.
            let status =
                unsafe { RegEnumKeyExW(self.0, index, PWSTR(name.as_mut_ptr()), &mut len, None, PWSTR::null(), None, None) };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            check(status, "enumerate subkeys")?;
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        Ok(out)
    }

    /// Deletes the subkey `name` (which must have no subkeys of its own). Missing is fine.
    fn delete_subkey(&self, name: &str) -> Result<()> {
        let name_w = wide(name);
        // SAFETY: `name_w` is NUL-terminated and outlives the call.
        let status = unsafe { RegDeleteKeyW(self.0, PCWSTR(name_w.as_ptr())) };
        if not_found(status) {
            return Ok(());
        }
        check(status, &format!("delete key {name}"))
    }
}

pub(crate) fn filetime(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

// ---------------------------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------------------------

/// What `apo_service` needs to know about one playback endpoint.
#[derive(Clone, Debug)]
pub(crate) struct EndpointInfo {
    /// The key name as the registry spells it.
    pub guid: String,
    /// "Speakers (Realtek(R) Audio)": what Windows shows, for the virtual-device check.
    pub name: String,
    /// `DeviceState` (1 active, 2 disabled, 4 not present, 8 unplugged).
    pub device_state: u32,
    pub state: DeviceRegState,
    /// Last write of `FxProperties` (FILETIME ticks), when the key exists.
    pub fx_last_write: Option<u64>,
    /// `Disable_SysFx` is DWORD 1 in `Properties` (older notes put the switch there; DeviceSelector uses `FxProperties`).
    pub sysfx_in_properties: bool,
}

impl EndpointInfo {
    pub fn present(&self) -> bool {
        self.device_state & DEVICE_NOT_PRESENT == 0
    }

    /// Windows "audio enhancements" are switched off here, so no APO runs on this endpoint.
    pub fn enhancements_off(&self) -> bool {
        self.sysfx_in_properties || self.state.fx.get(&SYSFX.to_ascii_lowercase()) == Some(&RegValue::Dword(1))
    }
}

/// The registry's own spelling of `guid`, after the two checks: a braced GUID, and an existing endpoint.
pub(crate) fn resolve_endpoint(guid: &str) -> Result<String> {
    if !is_braced_guid(guid) {
        user_bail!("Identificador de dispositivo inválido.");
    }
    let render = Key::open(RENDER, KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE)?.context("the Render endpoint key is missing")?;
    render
        .subkeys()?
        .into_iter()
        .find(|k| k.eq_ignore_ascii_case(guid))
        .ok_or_else(|| user_err("Aparelho de áudio não encontrado."))
}

fn read_endpoint(guid: &str) -> Result<EndpointInfo> {
    let endpoint_path = format!(r"{RENDER}\{guid}");
    let endpoint = Key::open(&endpoint_path, KEY_QUERY_VALUE)?.ok_or_else(|| user_err("Aparelho de áudio não encontrado."))?;
    let device_state = match endpoint.get("DeviceState")? {
        Some(RegValue::Dword(n)) => n,
        _ => 0,
    };
    let mut name_parts = Vec::new();
    let (mut combined, mut sysfx_in_properties) = (false, false);
    if let Some(props) = Key::open(&format!(r"{endpoint_path}\Properties"), KEY_QUERY_VALUE)? {
        for n in [PROP_CONNECTION, PROP_DEVICE] {
            if let Some(RegValue::Str(v)) = props.get(n)? {
                name_parts.push(v);
            }
        }
        combined = props.get(COMBINED_DEVICE)?.is_some();
        sysfx_in_properties = props.get(SYSFX)? == Some(RegValue::Dword(1));
    }
    let name = match name_parts.as_slice() {
        [a, b] => format!("{a} ({b})"),
        [a] => a.clone(),
        _ => String::new(),
    };
    let mut state = DeviceRegState { combined, ..DeviceRegState::default() };
    let mut fx_last_write = None;
    if let Some(fx) = Key::open(&format!(r"{endpoint_path}\FxProperties"), KEY_QUERY_VALUE)? {
        state.fx_key_exists = true;
        fx_last_write = Some(fx.info()?.4);
        for n in tracked_fx_names() {
            if let Some(v) = fx.get(n)? {
                state.fx.insert(n.to_ascii_lowercase(), v);
            }
        }
    }
    if let Some(child) = Key::open(&format!(r"{CHILD_ROOT}\{guid}"), KEY_QUERY_VALUE)? {
        state.child = Some(child.values()?.into_iter().map(|(n, v)| (n.to_ascii_lowercase(), v)).collect::<Values>());
    }
    Ok(EndpointInfo { guid: guid.to_string(), name, device_state, state, fx_last_write, sysfx_in_properties })
}

/// One endpoint, validated (see `resolve_endpoint`).
pub(crate) fn read_device(guid: &str) -> Result<EndpointInfo> {
    read_endpoint(&resolve_endpoint(guid)?)
}

/// Every playback endpoint that is not "not present"; one that cannot be read is logged and left out.
pub(crate) fn list_devices() -> Result<Vec<EndpointInfo>> {
    let render = Key::open(RENDER, KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE)?.context("the Render endpoint key is missing")?;
    let mut out = Vec::new();
    for guid in render.subkeys()?.into_iter().filter(|g| is_braced_guid(g)) {
        match read_endpoint(&guid) {
            Ok(info) if info.present() => out.push(info),
            Ok(_) => {}
            Err(e) => log::debug!("apo: cannot read endpoint {guid}: {e:#}"),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Applying a plan
// ---------------------------------------------------------------------------------------------

/// Executes `plan` on `guid` (already resolved: an existing endpoint key name), in order, stopping at the
/// first error. After a `DeleteChildKey` the (then maybe empty) `Child APOs` key is removed too, as
/// DeviceSelector does.
pub(crate) fn apply_plan(guid: &str, plan: &Plan) -> Result<()> {
    let endpoint_path = format!(r"{RENDER}\{guid}");
    let fx_path = format!(r"{endpoint_path}\FxProperties");
    let child_path = format!(r"{CHILD_ROOT}\{guid}");
    let mut fx: Option<Key> = None;
    let mut child: Option<Key> = None;
    let write_access = KEY_SET_VALUE | KEY_QUERY_VALUE;

    for op in &plan.ops {
        match op {
            Op::CreateFxKey => fx = Some(Key::create(&fx_path, write_access)?),
            Op::SetFx(name, value) => lazy(&mut fx, &fx_path, "FxProperties")?.set(name, value)?,
            Op::DeleteFx(name) => lazy(&mut fx, &fx_path, "FxProperties")?.delete_value(name)?,
            Op::DeleteFxKey => {
                fx = None; // close our handle first: a key is only removed when every handle is closed
                let endpoint = Key::open(&endpoint_path, KEY_QUERY_VALUE)?.context("the endpoint key disappeared")?;
                endpoint.delete_subkey("FxProperties")?;
            }
            Op::CreateChildKey => child = Some(Key::create(&child_path, write_access)?),
            Op::SetChild(name, value) => lazy(&mut child, &child_path, "the APO record")?.set(name, value)?,
            Op::DeleteChild(name) => lazy(&mut child, &child_path, "the APO record")?.delete_value(name)?,
            Op::DeleteChildKey => {
                child = None;
                let root = Key::open(CHILD_ROOT, KEY_QUERY_VALUE | KEY_SET_VALUE)?;
                if let Some(root) = root {
                    root.delete_subkey(guid)?;
                    let (subkeys, values, ..) = root.info()?;
                    drop(root);
                    if subkeys == 0 && values == 0 {
                        let apo = Key::open(APO_ROOT, KEY_QUERY_VALUE)?.context("the APO key disappeared")?;
                        apo.delete_subkey("Child APOs")?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The key in `slot`, opened for writing on first use; it must exist (`CreateFxKey` / `CreateChildKey` made it).
fn lazy<'a>(slot: &'a mut Option<Key>, path: &str, what: &str) -> Result<&'a Key> {
    if slot.is_none() {
        *slot = Some(Key::open(path, KEY_SET_VALUE | KEY_QUERY_VALUE)?.with_context(|| format!("{what} does not exist"))?);
    }
    slot.as_ref().context("registry key not open")
}

// ---------------------------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------------------------

fn value_json(name: &str, v: &RegValue) -> serde_json::Value {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    match v {
        RegValue::Str(s) => json!({"name": name, "type": "REG_SZ", "data": s}),
        RegValue::MultiStr(items) => json!({"name": name, "type": "REG_MULTI_SZ", "data": items}),
        RegValue::Dword(n) => json!({"name": name, "type": "REG_DWORD", "data": n}),
        RegValue::Binary(b) => json!({"name": name, "type": "REG_BINARY", "data": hex(b)}),
        RegValue::Raw(ty, b) => json!({"name": name, "type": format!("type {ty}"), "data": hex(b)}),
    }
}

/// The backup text of one endpoint: every value of its `FxProperties` and of APO's record, as they are
/// right now (`None` = the key does not exist). JSON, so a person can read it and a script can restore it.
pub(crate) fn backup_json(guid: &str, saved_at_unix: u64, fx: Option<&[(String, RegValue)]>, child: Option<&[(String, RegValue)]>) -> String {
    let list = |v: Option<&[(String, RegValue)]>| match v {
        Some(items) => json!(items.iter().map(|(n, v)| value_json(n, v)).collect::<Vec<_>>()),
        None => serde_json::Value::Null,
    };
    let doc = json!({
        "about": "Clarim backup of this playback device's effect settings, taken before its first change. Registry paths: HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\Render\\<guid>\\FxProperties and HKLM\\SOFTWARE\\EqualizerAPO\\Child APOs\\<guid>.",
        "guid": guid,
        "saved_at_unix": saved_at_unix,
        "fx_properties": list(fx),
        "child_apos": list(child),
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// Writes `<dir>\<guid>.json` with the endpoint's current values, unless a backup of it exists already
/// (the first one, from before any change, is the one worth keeping).
pub(crate) fn backup_device(dir: &Path, guid: &str) -> Result<()> {
    let target = dir.join(format!("{guid}.json"));
    if target.exists() {
        return Ok(());
    }
    let read_all = |path: String| -> Result<Option<Vec<(String, RegValue)>>> {
        Ok(match Key::open(&path, KEY_QUERY_VALUE)? {
            Some(k) => Some(k.values()?),
            None => None,
        })
    };
    let fx = read_all(format!(r"{RENDER}\{guid}\FxProperties"))?;
    let child = read_all(format!(r"{CHILD_ROOT}\{guid}"))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let text = backup_json(guid, now, fx.as_deref(), child.as_deref());
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // create_new: never overwrites, even when two requests race.
    let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&target) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot create {}", target.display())),
    };
    file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).with_context(|| format!("cannot write {}", target.display()))
}

// ---------------------------------------------------------------------------------------------
// Global switches Equalizer APO needs
// ---------------------------------------------------------------------------------------------

/// `DisableProtectedAudioDG` = 1: without it Windows refuses the (unsigned) APO in the audio graph.
/// Set the way DeviceSelector does (`checkProtectedAudioDG(true)`); returns whether it had to be written.
pub(crate) fn ensure_protected_audio_dg() -> Result<bool> {
    let key = Key::create(AUDIO, KEY_SET_VALUE | KEY_QUERY_VALUE)?;
    if key.get("DisableProtectedAudioDG")? == Some(RegValue::Dword(1)) {
        return Ok(false);
    }
    key.set("DisableProtectedAudioDG", &RegValue::Dword(1))?;
    Ok(true)
}

/// `HKLM\SOFTWARE\EqualizerAPO` exists with an `InstallPath`: the installer ran.
pub(crate) fn apo_key_present() -> bool {
    matches!(Key::open(APO_ROOT, KEY_QUERY_VALUE), Ok(Some(k)) if matches!(k.get("InstallPath"), Ok(Some(_))))
}

/// The two classes are registered the way `regsvr32` leaves them (what DeviceSelector's
/// `checkAPORegistration` looks for). Read-only.
pub(crate) fn apo_registered() -> bool {
    [crate::apo_plan::APO_PRE, crate::apo_plan::APO_POST].iter().all(|clsid| {
        let exists = |path: String| matches!(Key::open(&path, KEY_READ), Ok(Some(_)));
        exists(format!(r"SOFTWARE\Classes\CLSID\{clsid}")) && exists(format!(r"SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\{clsid}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_ids_are_checked_before_the_registry_is_touched() {
        for bad in [
            "",
            "{x}",
            "5D16E8DA-37A3-4657-8D52-57FFE97F4EF9",
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\\..\\x",
            "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\n",
            "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}",
        ] {
            assert_eq!(resolve_endpoint(bad).unwrap_err().to_string(), "Identificador de dispositivo inválido.", "{bad:?}");
        }
    }

    #[test]
    fn a_well_formed_guid_that_is_not_an_endpoint_is_refused() {
        // read-only: enumerates the Render key
        let err = resolve_endpoint("{00000000-0000-0000-0000-00000000dead}").unwrap_err();
        assert_eq!(err.to_string(), "Aparelho de áudio não encontrado.");
        assert!(read_device("{00000000-0000-0000-0000-00000000dead}").is_err());
    }

    #[test]
    fn the_backup_is_json_with_every_value_and_its_type() {
        let fx = vec![
            ("{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},5".to_string(), RegValue::Str("{62dc1a93-ae24-464c-a43e-452f824c4250}".into())),
            ("modes".to_string(), RegValue::MultiStr(vec!["{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}".into()])),
            ("flag".to_string(), RegValue::Dword(1)),
            ("blob".to_string(), RegValue::Binary(vec![0xde, 0xad])),
        ];
        let text = backup_json("{5d16e8da-37a3-4657-8d52-57ffe97f4ef9}", 1_700_000_000, Some(&fx), None);
        let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(v["guid"], "{5d16e8da-37a3-4657-8d52-57ffe97f4ef9}");
        assert_eq!(v["saved_at_unix"], 1_700_000_000u64);
        assert_eq!(v["fx_properties"][0]["type"], "REG_SZ");
        assert_eq!(v["fx_properties"][0]["data"], "{62dc1a93-ae24-464c-a43e-452f824c4250}");
        assert_eq!(v["fx_properties"][1]["data"][0], "{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}");
        assert_eq!(v["fx_properties"][2]["data"], 1);
        assert_eq!(v["fx_properties"][3]["data"], "dead");
        assert!(v["child_apos"].is_null(), "a missing key is null, not an empty list");
    }

    #[test]
    fn a_backup_is_written_once_and_never_overwritten() {
        // The file logic only: the registry reads for a missing endpoint are empty (null lists).
        let dir = std::env::temp_dir().join(format!("clarim-apo-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let guid = "{00000000-0000-0000-0000-00000000dead}";
        backup_device(&dir, guid).expect("first backup");
        let file = dir.join(format!("{guid}.json"));
        let first = fs::read_to_string(&file).expect("backup file");
        assert!(first.contains("\"fx_properties\": null"), "{first}");
        fs::write(&file, "kept").unwrap();
        backup_device(&dir, guid).expect("second call is a no-op");
        assert_eq!(fs::read_to_string(&file).unwrap(), "kept", "the first backup is the one that stays");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Read-only: classifies every playback endpoint of THIS PC.
    /// `cargo test -p clarim-svc live_devices -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the real registry"]
    fn live_devices() {
        for d in list_devices().expect("devices") {
            println!(
                "{} {:<45} state {} class {:?} combined {} enh_off {} fx_write {:?}",
                d.guid,
                d.name,
                d.device_state,
                crate::apo_plan::classify(&d.state),
                d.state.combined,
                d.enhancements_off(),
                d.fx_last_write
            );
            if let Some(child) = &d.state.child {
                println!("    record: {child:?}");
            }
        }
        println!("apo key present: {}  registered: {}", apo_key_present(), apo_registered());
    }
}
