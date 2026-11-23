//! Pure model of what Equalizer APO's DeviceSelector 1.4.2 does to one playback endpoint's registry
//! state, so Clarim can attach / detach the APO without ever opening DeviceSelector.
//!
//! Nothing here touches the registry: `plan_install` / `plan_uninstall` take the values read from the
//! endpoint (`DeviceRegState`) and return an ordered `Plan` of set / delete operations; `apo_reg`
//! executes a plan. Source of every rule: `DeviceAPOInfo::{load,install,uninstall}` in the official
//! EqualizerAPO-src-1.4.2 (docs/eqapo.md has the mapping). Where Clarim deliberately differs from
//! DeviceSelector, the line says so ("Clarim:").
//!
//! Registry layout (HKLM):
//! - `SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{guid}\FxProperties` holds the
//!   endpoint's effect slots `{d04e05a6-...},N` (1 LFX, 2 GFX, 5 SFX, 6 MFX, 7 EFX) as REG_SZ class ids.
//! - `SOFTWARE\EqualizerAPO\Child APOs\{guid}` is APO's record of the original slot values (`!VALUE` =
//!   the value did not exist, `!KEY` = the whole FxProperties key did not exist), the class ids it
//!   chains to (`PreMixChild` / `PostMixChild`), `AllowSilentBufferModification` and `Version` = `2`.
//!
//! All map keys are lower-case (registry names are case-insensitive); operations carry the canonical
//! spelling for the registry and are lower-cased again by `simulate`.

use std::collections::BTreeMap;
use std::fmt;

/// Equalizer APO's pre-mix and post-mix class ids, as DeviceSelector writes them (upper case).
pub const APO_PRE: &str = "{EACD2258-FCAC-4FF4-B36D-419E924A6D79}";
pub const APO_POST: &str = "{EC1CC9CE-FAED-4822-828A-82A81A6F018F}";

const LFX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},1";
const GFX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},2";
const SFX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},5";
const MFX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},6";
const EFX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},7";
/// The five slots, in DeviceSelector's order (LFX, GFX, SFX, MFX, EFX).
const SLOTS: [&str; 5] = [LFX, GFX, SFX, MFX, EFX];
/// Multi-APO slots: read by DeviceSelector to pick the install mode, never written.
const MULTI_SLOTS: [&str; 3] = [
    "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},13",
    "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},14",
    "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},15",
];
const MODES_SFX: &str = "{d3993a3f-99c2-4402-b5ec-a92a0367664b},5";
const MODES_MFX: &str = "{d3993a3f-99c2-4402-b5ec-a92a0367664b},6";
const MODES_EFX: &str = "{d3993a3f-99c2-4402-b5ec-a92a0367664b},7";
const DEFAULT_MODE: &str = "{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}";
const FX_TITLE: &str = "{b725f130-47ef-101a-a5f1-02608c9eebac},10";
/// PKEY_AudioEndpoint_Disable_SysFx: DWORD 1 = Windows "audio enhancements" off. DeviceSelector reads and
/// deletes it in `FxProperties`.
pub const SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";

const NO_VALUE: &str = "!VALUE";
const NO_KEY: &str = "!KEY";

pub const CHILD_PRE: &str = "PreMixChild";
pub const CHILD_POST: &str = "PostMixChild";
const CHILD_SILENT: &str = "AllowSilentBufferModification";
const CHILD_AUTO_ADJUST: &str = "DisableAutomaticAdjustment";
const CHILD_VERSION: &str = "Version";
/// Clarim: which `{d3993a3f-...}` processing-mode values this install created (e.g. `5,7`). DeviceSelector
/// never removes them; with this record `plan_uninstall` can, so install then uninstall is an exact round
/// trip. APO ignores unknown values in its record.
const CHILD_MARK_MODES: &str = "ClarimCreatedModes";
/// Clarim: the DWORD `Disable_SysFx` had before install deleted it ("audio enhancements" switch), as text.
/// DeviceSelector does not restore it; `plan_uninstall` does.
const CHILD_MARK_SYSFX: &str = "ClarimSysFxWas";

/// Names (lower case) the registry layer must read from `FxProperties`; everything else is not our business.
pub fn tracked_fx_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = SLOTS.to_vec();
    v.extend(MULTI_SLOTS);
    v.extend([MODES_SFX, MODES_MFX, MODES_EFX, FX_TITLE, SYSFX]);
    v
}

/// `{b3f8fa53-...},41` in the endpoint's `Properties`: set on Bluetooth devices Windows 11 shows as one
/// combined device; the EFX slot does not work there.
pub const COMBINED_DEVICE: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},41";

// ---------------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------------

/// A registry value, typed. `Raw` keeps any other type bit for bit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RegValue {
    Str(String),
    MultiStr(Vec<String>),
    Dword(u32),
    Binary(Vec<u8>),
    Raw(u32, Vec<u8>),
}

const REG_SZ: u32 = 1;
const REG_BINARY: u32 = 3;
const REG_DWORD: u32 = 4;
const REG_MULTI_SZ: u32 = 7;

fn utf16_units(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

impl RegValue {
    /// Value of registry type `ty` from the raw bytes `RegQueryValueEx` / `RegEnumValue` return.
    pub fn decode(ty: u32, data: &[u8]) -> RegValue {
        match ty {
            REG_SZ => {
                let units = utf16_units(data);
                let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
                RegValue::Str(String::from_utf16_lossy(&units[..end]))
            }
            REG_MULTI_SZ => {
                let units = utf16_units(data);
                let mut items: Vec<String> = units.split(|&u| u == 0).map(String::from_utf16_lossy).collect();
                while items.last().is_some_and(String::is_empty) {
                    items.pop();
                }
                RegValue::MultiStr(items)
            }
            REG_DWORD if data.len() == 4 => RegValue::Dword(u32::from_le_bytes([data[0], data[1], data[2], data[3]])),
            REG_BINARY => RegValue::Binary(data.to_vec()),
            _ => RegValue::Raw(ty, data.to_vec()),
        }
    }

    /// `(registry type, bytes)` for `RegSetValueEx`.
    pub fn encode(&self) -> (u32, Vec<u8>) {
        let bytes = |units: &[u16]| units.iter().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>();
        match self {
            RegValue::Str(s) => (REG_SZ, bytes(&s.encode_utf16().chain(Some(0)).collect::<Vec<_>>())),
            RegValue::MultiStr(items) => {
                let mut units: Vec<u16> = items.iter().flat_map(|s| s.encode_utf16().chain(Some(0))).collect();
                units.push(0);
                if items.is_empty() {
                    units.push(0);
                }
                (REG_MULTI_SZ, bytes(&units))
            }
            RegValue::Dword(n) => (REG_DWORD, n.to_le_bytes().to_vec()),
            RegValue::Binary(b) => (REG_BINARY, b.clone()),
            RegValue::Raw(ty, b) => (*ty, b.clone()),
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            RegValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Values of one registry key, keyed by lower-case name.
pub type Values = BTreeMap<String, RegValue>;

fn key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// What the registry layer read for one endpoint. Absent = not in the map.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DeviceRegState {
    /// `...\{guid}\FxProperties` exists.
    pub fx_key_exists: bool,
    /// The tracked values of `FxProperties` (see `tracked_fx_names`).
    pub fx: Values,
    /// `Properties` has `COMBINED_DEVICE`.
    pub combined: bool,
    /// `HKLM\SOFTWARE\EqualizerAPO\Child APOs\{guid}`: all its values, `None` when the key does not exist.
    pub child: Option<Values>,
}

// ---------------------------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------------------------

/// How Equalizer APO is attached to an endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Attach {
    /// No slot holds an APO class id.
    Free,
    /// Attached, and APO's record of the originals is there (`Version` 2): can be undone.
    Installed,
    /// Attached, but APO's record is missing (a half-done or foreign install): the originals are unknown.
    Orphan,
    /// Attached, with a record of another version: not understood, left alone.
    Unsupported,
}

fn is_apo_clsid(s: &str) -> bool {
    let s = s.trim();
    s.eq_ignore_ascii_case(APO_PRE) || s.eq_ignore_ascii_case(APO_POST)
}

pub fn classify(state: &DeviceRegState) -> Attach {
    let attached = SLOTS.iter().any(|slot| state.fx.get(*slot).and_then(RegValue::as_str).is_some_and(is_apo_clsid));
    if !attached {
        return Attach::Free;
    }
    match &state.child {
        None => Attach::Orphan,
        Some(child) => match child.get(&key(CHILD_VERSION)).and_then(RegValue::as_str) {
            Some("2") => Attach::Installed,
            // Version 1 (no value) and anything newer: DeviceSelector would upgrade or refuse; Clarim leaves them.
            _ => Attach::Unsupported,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------------------------

/// One registry operation. `*Fx*` act on the endpoint's `FxProperties`, `*Child*` on APO's record
/// `Child APOs\{guid}` (`CreateChildKey` also creates `Child APOs` when missing).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    CreateFxKey,
    SetFx(String, RegValue),
    DeleteFx(String),
    DeleteFxKey,
    CreateChildKey,
    SetChild(String, RegValue),
    DeleteChild(String),
    DeleteChildKey,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Plan {
    pub ops: Vec<Op>,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.ops.is_empty()
    }

    /// The registry state after the plan, as a model of what `apo_reg::apply` does. A set before the key
    /// exists is a bug in the plan (the real registry refuses it too), so it panics.
    #[cfg(test)]
    pub fn simulate(&self, state: &mut DeviceRegState) {
        for op in &self.ops {
            match op {
                Op::CreateFxKey => state.fx_key_exists = true,
                Op::SetFx(name, value) => {
                    assert!(state.fx_key_exists, "SetFx({name}) before the FxProperties key exists");
                    state.fx.insert(key(name), value.clone());
                }
                Op::DeleteFx(name) => {
                    state.fx.remove(&key(name));
                }
                Op::DeleteFxKey => {
                    state.fx_key_exists = false;
                    state.fx.clear();
                }
                Op::CreateChildKey => {
                    state.child.get_or_insert_with(Values::new);
                }
                Op::SetChild(name, value) => {
                    let child = state.child.as_mut().unwrap_or_else(|| panic!("SetChild({name}) before the record exists"));
                    child.insert(key(name), value.clone());
                }
                Op::DeleteChild(name) => {
                    if let Some(child) = state.child.as_mut() {
                        child.remove(&key(name));
                    }
                }
                Op::DeleteChildKey => state.child = None,
            }
        }
    }
}

/// Why no plan could be made. The text is PT-BR, for the dashboard.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// A slot holds a value that is not a string: Clarim will not overwrite what it does not understand.
    UnexpectedValue(String),
    /// The APO is attached but its record of the original values is missing.
    NoRecord,
    /// The record was made by another Equalizer APO version.
    UnsupportedRecord,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::UnexpectedValue(_) => f.write_str("Este aparelho tem uma configuração de efeitos que o Clarim não entende; nada foi alterado."),
            PlanError::NoRecord => f.write_str(
                "O Equalizer APO está ligado neste aparelho sem o registro dos valores originais; o Clarim não consegue desfazer. Use o desinstalador do Equalizer APO.",
            ),
            PlanError::UnsupportedRecord => f.write_str("Este aparelho foi configurado por outra versão do Equalizer APO; o Clarim não mexe nele."),
        }
    }
}

impl std::error::Error for PlanError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    LfxGfx,
    SfxMfx,
    SfxEfx,
}

/// DeviceSelector's default mode for a device that is not installed yet (Windows 8.1+).
fn pick_mode(state: &DeviceRegState) -> Mode {
    let has = |name: &str| state.fx.contains_key(name);
    let only_lfx_gfx = (has(LFX) || has(GFX)) && !has(SFX) && !has(MFX) && !has(EFX) && !MULTI_SLOTS.iter().any(|n| has(n));
    if only_lfx_gfx {
        Mode::LfxGfx
    } else if state.combined {
        Mode::SfxMfx
    } else {
        Mode::SfxEfx
    }
}

fn s(v: &str) -> RegValue {
    RegValue::Str(v.to_string())
}

/// Original slot values as DeviceSelector records them: the string, `!VALUE` or `!KEY`.
fn originals(state: &DeviceRegState) -> Result<[String; 5], PlanError> {
    let mut out: [String; 5] = Default::default();
    for (slot, orig) in SLOTS.iter().zip(out.iter_mut()) {
        *orig = if !state.fx_key_exists {
            NO_KEY.to_string()
        } else {
            match state.fx.get(*slot) {
                None => NO_VALUE.to_string(),
                Some(RegValue::Str(v)) => v.clone(),
                Some(_) => return Err(PlanError::UnexpectedValue((*slot).to_string())),
            }
        };
    }
    Ok(out)
}

/// `getOriginalAPOPreMix` / `getOriginalAPOPostMix`: the class ids APO chains to, `""` for none.
fn children(mode: Mode, o: &[String; 5]) -> (String, String) {
    let (lfx, gfx, sfx, mfx, efx) = (&o[0], &o[1], &o[2], &o[3], &o[4]);
    let nv = |a: &String, b: &String| a == NO_VALUE && b == NO_VALUE;
    let (pre, post) = match mode {
        Mode::LfxGfx => (if nv(lfx, gfx) { sfx } else { lfx }, if nv(lfx, gfx) { mfx } else { gfx }),
        Mode::SfxMfx => (if nv(sfx, mfx) { lfx } else { sfx }, if nv(sfx, mfx) { gfx } else { mfx }),
        Mode::SfxEfx => (if nv(sfx, efx) { lfx } else { sfx }, if nv(sfx, efx) { gfx } else { efx }),
    };
    let real = |g: &String| if g == NO_KEY || g == NO_VALUE { String::new() } else { g.clone() };
    (real(pre), real(post))
}

fn default_modes() -> RegValue {
    RegValue::MultiStr(vec![DEFAULT_MODE.to_string()])
}

fn is_default_modes(v: &RegValue) -> bool {
    matches!(v, RegValue::MultiStr(items) if items.len() == 1 && items[0].eq_ignore_ascii_case(DEFAULT_MODE))
}

/// Attaches APO to the endpoint, as DeviceSelector's "install" does for a render device with the default
/// options (use the original APO as child, no silent-buffer modification, automatic adjustment on).
/// Already attached (`Installed` or `Orphan`) gives an empty plan: nothing to do, and an orphan has no
/// record to build on, so it is left as it is (Clarim: DeviceSelector would reinstall it).
pub fn plan_install(state: &DeviceRegState) -> Result<Plan, PlanError> {
    match classify(state) {
        Attach::Installed | Attach::Orphan => return Ok(Plan::default()),
        Attach::Unsupported => return Err(PlanError::UnsupportedRecord),
        Attach::Free => {}
    }
    let orig = originals(state)?;
    let mode = pick_mode(state);
    let (pre, post) = children(mode, &orig);
    let has = |name: &str| state.fx.contains_key(name);
    let stale = state.child.as_ref(); // a record left behind by an install Windows has undone since

    // FxProperties writes, decided first because the record mentions some of them.
    let mut fx = Vec::new();
    let mut created_modes: Vec<&str> = Vec::new();
    let mut set_slot = |fx: &mut Vec<Op>, slot: &str, clsid: &str, modes: Option<(&str, &'static str)>| {
        fx.push(Op::SetFx(slot.to_string(), s(clsid)));
        if let Some((name, tag)) = modes {
            if !state.fx.contains_key(name) {
                fx.push(Op::SetFx(name.to_string(), default_modes()));
                created_modes.push(tag);
            }
        }
    };
    let delete_if_present = |fx: &mut Vec<Op>, names: &[&str]| {
        for n in names {
            if has(n) {
                fx.push(Op::DeleteFx((*n).to_string()));
            }
        }
    };
    match mode {
        Mode::LfxGfx => {
            set_slot(&mut fx, LFX, APO_PRE, None);
            set_slot(&mut fx, GFX, APO_POST, None);
            delete_if_present(&mut fx, &[SFX, MFX, EFX]);
        }
        Mode::SfxMfx => {
            delete_if_present(&mut fx, &[LFX, GFX]);
            set_slot(&mut fx, SFX, APO_PRE, Some((MODES_SFX, "5")));
            set_slot(&mut fx, MFX, APO_POST, Some((MODES_MFX, "6")));
        }
        Mode::SfxEfx => {
            delete_if_present(&mut fx, &[LFX, GFX]);
            set_slot(&mut fx, SFX, APO_PRE, Some((MODES_SFX, "5")));
            set_slot(&mut fx, EFX, APO_POST, Some((MODES_EFX, "7")));
        }
    }
    // "force-enable enhancements": DeviceSelector deletes the switch; Clarim also remembers a DWORD value.
    let sysfx_was = match state.fx.get(SYSFX) {
        Some(RegValue::Dword(n)) => Some(n.to_string()),
        _ => None,
    };
    delete_if_present(&mut fx, &[SYSFX]);

    // The record first, complete, so an interrupted install leaves something `plan_uninstall` can use.
    let mut ops = vec![Op::CreateChildKey];
    for (slot, o) in SLOTS.iter().zip(&orig) {
        ops.push(Op::SetChild((*slot).to_string(), s(o)));
    }
    ops.push(Op::SetChild(CHILD_PRE.into(), s(&pre)));
    ops.push(Op::SetChild(CHILD_POST.into(), s(&post)));
    ops.push(Op::SetChild(CHILD_SILENT.into(), s("false")));
    // autoAdjust on: the flag is deleted (a stale record may still have it).
    let stale_has = |name: &str| stale.is_some_and(|c| c.contains_key(&key(name)));
    if stale_has(CHILD_AUTO_ADJUST) {
        ops.push(Op::DeleteChild(CHILD_AUTO_ADJUST.into()));
    }
    for (name, value) in [(CHILD_MARK_MODES, (!created_modes.is_empty()).then(|| created_modes.join(","))), (CHILD_MARK_SYSFX, sysfx_was)] {
        match value {
            Some(v) => ops.push(Op::SetChild(name.into(), s(&v))),
            None if stale_has(name) => ops.push(Op::DeleteChild(name.into())),
            None => {}
        }
    }
    ops.push(Op::SetChild(CHILD_VERSION.into(), s("2")));

    if !state.fx_key_exists {
        // DeviceSelector: create the key and give it the title "Equalizer APO".
        ops.push(Op::CreateFxKey);
        ops.push(Op::SetFx(FX_TITLE.into(), s("Equalizer APO")));
    }
    ops.extend(fx);
    Ok(Plan { ops })
}

fn modes_name(tag: &str) -> Option<&'static str> {
    match tag {
        "5" => Some(MODES_SFX),
        "6" => Some(MODES_MFX),
        "7" => Some(MODES_EFX),
        _ => None,
    }
}

/// Detaches APO from the endpoint, as DeviceSelector's "uninstall" does: every slot goes back to the value
/// APO recorded (`!VALUE` = delete it), a recorded `!KEY` deletes the whole `FxProperties`, and the record
/// is deleted. Clarim also: deletes the processing-mode values its own install created, puts the
/// "audio enhancements" switch back, and deletes a slot that still holds an APO class id when the record
/// has nothing for it. An endpoint with no APO and no stale record gives an empty plan; with a stale record,
/// a plan that deletes it.
pub fn plan_uninstall(state: &DeviceRegState) -> Result<Plan, PlanError> {
    match classify(state) {
        Attach::Free => {
            let ops = if state.child.is_some() { vec![Op::DeleteChildKey] } else { Vec::new() };
            return Ok(Plan { ops });
        }
        Attach::Orphan => return Err(PlanError::NoRecord),
        Attach::Unsupported => return Err(PlanError::UnsupportedRecord),
        Attach::Installed => {}
    }
    let Some(child) = &state.child else { return Err(PlanError::NoRecord) };
    let recorded = |name: &str| child.get(&key(name)).and_then(RegValue::as_str);

    let mut ops = Vec::new();
    if recorded(LFX) == Some(NO_KEY) {
        ops.push(Op::DeleteFxKey);
    } else {
        for slot in SLOTS {
            let current = state.fx.get(slot).and_then(RegValue::as_str);
            match recorded(slot) {
                Some(NO_VALUE) => {
                    if state.fx.contains_key(slot) {
                        ops.push(Op::DeleteFx(slot.to_string()));
                    }
                }
                Some(original) if !original.is_empty() => {
                    if current != Some(original) {
                        ops.push(Op::SetFx(slot.to_string(), s(original)));
                    }
                }
                _ => {
                    if current.is_some_and(is_apo_clsid) {
                        ops.push(Op::DeleteFx(slot.to_string()));
                    }
                }
            }
        }
        for tag in recorded(CHILD_MARK_MODES).unwrap_or_default().split(',') {
            if let Some(name) = modes_name(tag) {
                if state.fx.get(name).is_some_and(is_default_modes) {
                    ops.push(Op::DeleteFx(name.to_string()));
                }
            }
        }
        if let Some(n) = recorded(CHILD_MARK_SYSFX).and_then(|v| v.parse::<u32>().ok()) {
            ops.push(Op::SetFx(SYSFX.to_string(), RegValue::Dword(n)));
        }
    }
    ops.push(Op::DeleteChildKey);
    Ok(Plan { ops })
}

/// The plan that takes the tracked state `now` back to `before` (rollback of a half-applied plan; also the
/// tool of the tests). FxProperties first, the record last: the reverse of `plan_install`.
pub fn plan_restore(before: &DeviceRegState, now: &DeviceRegState) -> Plan {
    let mut ops = Vec::new();
    if !before.fx_key_exists {
        if now.fx_key_exists {
            ops.push(Op::DeleteFxKey);
        }
    } else {
        if !now.fx_key_exists {
            ops.push(Op::CreateFxKey);
        }
        let names: std::collections::BTreeSet<&String> = before.fx.keys().chain(now.fx.keys()).collect();
        for name in names {
            match (before.fx.get(name), now.fx.get(name)) {
                (Some(b), Some(n)) if b == n => {}
                (Some(b), _) => ops.push(Op::SetFx(name.clone(), b.clone())),
                (None, Some(_)) => ops.push(Op::DeleteFx(name.clone())),
                (None, None) => {}
            }
        }
    }
    match (&before.child, &now.child) {
        (None, Some(_)) => ops.push(Op::DeleteChildKey),
        (None, None) => {}
        (Some(b), now_child) => {
            if now_child.is_none() {
                ops.push(Op::CreateChildKey);
            }
            let empty = Values::new();
            let n = now_child.as_ref().unwrap_or(&empty);
            for name in b.keys().chain(n.keys()).collect::<std::collections::BTreeSet<_>>() {
                match (b.get(name), n.get(name)) {
                    (Some(bv), Some(nv)) if bv == nv => {}
                    (Some(bv), _) => ops.push(Op::SetChild(name.clone(), bv.clone())),
                    (None, Some(_)) => ops.push(Op::DeleteChild(name.clone())),
                    (None, None) => {}
                }
            }
        }
    }
    Plan { ops }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRE: &str = "{EACD2258-FCAC-4FF4-B36D-419E924A6D79}";
    const POST: &str = "{EC1CC9CE-FAED-4822-828A-82A81A6F018F}";
    const MODE: &str = "{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}";
    const TITLE: &str = "{b725f130-47ef-101a-a5f1-02608c9eebac},10";
    const SLOT3: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},3";
    const SLOT0: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},0";
    const REALTEK_SFX: &str = "{62dc1a93-ae24-464c-a43e-452f824c4250}";
    const REALTEK_MFX: &str = "{637c490d-eee3-4c0a-973f-371958802da2}";
    const BT_SFX: &str = "{C9453E73-8C5C-4463-9984-AF8BAB2F5447}";
    const BT_MFX: &str = "{13AB3EBD-137E-4903-9D89-60BE8277FD17}";
    const NULL_GUID: &str = "{00000000-0000-0000-0000-000000000000}";

    fn modes() -> RegValue {
        default_modes()
    }

    fn values(entries: &[(&str, RegValue)]) -> Values {
        entries.iter().map(|(n, v)| (key(n), v.clone())).collect()
    }

    fn with_fx(entries: &[(&str, RegValue)]) -> DeviceRegState {
        DeviceRegState { fx_key_exists: true, fx: values(entries), combined: false, child: None }
    }

    /// Ground truth, Realtek speakers, before DeviceSelector: the backup .reg holds ,5 and ,6; the title,
    /// ,3, ,0 and the SFX/MFX processing modes are what the dump shows as untouched.
    fn realtek_before() -> DeviceRegState {
        with_fx(&[
            (TITLE, s("Microsoft Audio Home Theater Effects")),
            (SFX, s(REALTEK_SFX)),
            (MFX, s(REALTEK_MFX)),
            (SLOT3, s("{5860E1C5-F95C-4a7a-8EC8-8AEF24F379A1}")),
            (SLOT0, s(NULL_GUID)),
            (MODES_SFX, modes()),
            (MODES_MFX, modes()),
        ])
    }

    /// The dump of the same device after DeviceSelector (FxProperties side).
    fn realtek_after_fx() -> Values {
        values(&[
            (TITLE, s("Microsoft Audio Home Theater Effects")),
            (SFX, s(PRE)),
            (MFX, s(REALTEK_MFX)),
            (EFX, s(POST)),
            (SLOT3, s("{5860E1C5-F95C-4a7a-8EC8-8AEF24F379A1}")),
            (SLOT0, s(NULL_GUID)),
            (MODES_SFX, modes()),
            (MODES_MFX, modes()),
            (MODES_EFX, modes()),
        ])
    }

    fn usb_before() -> DeviceRegState {
        let mut st = realtek_before();
        st.fx.insert(key(SLOT0), s("{DFF21CE1-F70F-11D0-B917-00A0C9223196}"));
        st
    }

    fn bt_before() -> DeviceRegState {
        with_fx(&[
            (TITLE, s("Bluetooth Headphones")),
            (SFX, s(BT_SFX)),
            (MFX, s(BT_MFX)),
            (SLOT3, s("{5860E1C5-F95C-4a7a-8EC8-8AEF24F379A1}")),
            ("{9c119480-ddc2-4954-a150-5bd240d454ad},7", RegValue::Binary(vec![1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0])),
            (MODES_SFX, modes()),
            (MODES_MFX, modes()),
        ])
    }

    /// The record without Clarim's own markers: what DeviceSelector would have written.
    fn apo_part(child: &Values) -> Values {
        child.iter().filter(|(k, _)| !k.starts_with("clarim")).map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn install(mut st: DeviceRegState) -> DeviceRegState {
        let plan = plan_install(&st).expect("plan");
        assert!(!plan.is_noop());
        plan.simulate(&mut st);
        st
    }

    fn round_trip(before: DeviceRegState) {
        let installed = install(before.clone());
        assert_eq!(classify(&installed), Attach::Installed);
        let plan = plan_uninstall(&installed).expect("uninstall plan");
        let mut after = installed;
        plan.simulate(&mut after);
        assert_eq!(after, before, "install then uninstall must restore the state exactly");
    }

    #[test]
    fn realtek_install_reproduces_the_dump() {
        let after = install(realtek_before());
        assert_eq!(after.fx, realtek_after_fx(), "FxProperties as the dump shows it");
        let child = after.child.as_ref().expect("record");
        assert_eq!(
            apo_part(child),
            values(&[
                (LFX, s("!VALUE")),
                (GFX, s("!VALUE")),
                (SFX, s(REALTEK_SFX)),
                (MFX, s(REALTEK_MFX)),
                (EFX, s("!VALUE")),
                ("PreMixChild", s(REALTEK_SFX)),
                ("PostMixChild", s("")),
                ("AllowSilentBufferModification", s("false")),
                ("Version", s("2")),
            ]),
            "Child APOs record as the dump shows it"
        );
        assert_eq!(child.get("clarimcreatedmodes"), Some(&s("7")), "only ,7 was created by this install");
        assert!(!child.contains_key("disableautomaticadjustment"));
    }

    #[test]
    fn usb_install_is_the_same_with_its_own_slot_zero() {
        let after = install(usb_before());
        let mut expected = realtek_after_fx();
        expected.insert(key(SLOT0), s("{DFF21CE1-F70F-11D0-B917-00A0C9223196}"));
        assert_eq!(after.fx, expected);
    }

    #[test]
    fn bluetooth_install_reproduces_the_dump() {
        let before = bt_before();
        let after = install(before.clone());
        let mut expected = before.fx.clone();
        expected.insert(key(SFX), s(PRE));
        expected.insert(key(EFX), s(POST));
        expected.insert(key(MODES_EFX), modes());
        assert_eq!(after.fx, expected, ",6 and the 12-byte value are left alone, ,7 is new");
        let child = apo_part(after.child.as_ref().expect("record"));
        assert_eq!(child[&key(SFX)], s(BT_SFX));
        assert_eq!(child[&key(MFX)], s(BT_MFX));
        assert_eq!(child[&key("PreMixChild")], s(BT_SFX));
        assert_eq!(child[&key("PostMixChild")], s(""));
        for slot in [LFX, GFX, EFX] {
            assert_eq!(child[&key(slot)], s("!VALUE"), "{slot}");
        }
    }

    #[test]
    fn install_then_uninstall_restores_every_before_state() {
        round_trip(realtek_before());
        round_trip(usb_before());
        round_trip(bt_before());
        // no FxProperties at all: created with the title, deleted whole by the uninstall (`!KEY`)
        round_trip(DeviceRegState::default());
        // "audio enhancements" switched off: install turns them on, uninstall puts the switch back
        let mut off = realtek_before();
        off.fx.insert(key(SYSFX), RegValue::Dword(1));
        let installed = install(off.clone());
        assert!(!installed.fx.contains_key(&key(SYSFX)), "install force-enables the enhancements");
        round_trip(off);
        let mut zero = realtek_before();
        zero.fx.insert(key(SYSFX), RegValue::Dword(0));
        round_trip(zero);
        // processing modes: none pre-existing (all three created, all three removed again)
        let mut bare = realtek_before();
        bare.fx.remove(&key(MODES_SFX));
        bare.fx.remove(&key(MODES_MFX));
        round_trip(bare);
        // a driver that supplies only LFX/GFX: LFX/GFX mode, restored too
        round_trip(with_fx(&[(LFX, s("{11111111-1111-1111-1111-111111111111}")), (GFX, s("{22222222-2222-2222-2222-222222222222}"))]));
        // a combined Bluetooth device: SFX/MFX mode
        let mut combined = bt_before();
        combined.combined = true;
        round_trip(combined);
        // a stale record from an install Windows undid, with flags a fresh install would not keep
        let mut stale = realtek_before();
        stale.child = Some(values(&[
            ("DisableAutomaticAdjustment", s("true")),
            ("ClarimCreatedModes", s("5,6,7")),
            ("ClarimSysFxWas", s("1")),
            ("Version", s("2")),
            ("Junk", s("x")),
        ]));
        let after = install(stale);
        let child = after.child.as_ref().expect("record");
        assert!(!child.contains_key("disableautomaticadjustment") && !child.contains_key("clarimsysfxwas"));
        assert_eq!(child["clarimcreatedmodes"], s("7"), "the markers describe THIS install");
        assert_eq!(child["junk"], s("x"), "values APO does not know are left in place");
    }

    #[test]
    fn modes_follow_devicesselectors_defaults() {
        // default: SFX + EFX
        let after = install(realtek_before());
        assert_eq!(after.fx[&key(EFX)], s(POST));
        assert_eq!(after.fx[&key(MFX)], s(REALTEK_MFX));
        // combined: SFX + MFX, EFX untouched
        let mut st = bt_before();
        st.combined = true;
        let after = install(st);
        assert_eq!((&after.fx[&key(SFX)], &after.fx[&key(MFX)]), (&s(PRE), &s(POST)));
        assert!(!after.fx.contains_key(&key(EFX)));
        let child = after.child.expect("record");
        assert_eq!(child[&key("PreMixChild")], s(BT_SFX));
        assert_eq!(child[&key("PostMixChild")], s(BT_MFX), "the original MFX is the post-mix child in SFX/MFX mode");
        // LFX/GFX only: LFX + GFX, the rest removed
        let after = install(with_fx(&[(LFX, s("{11111111-1111-1111-1111-111111111111}")), (GFX, s("{22222222-2222-2222-2222-222222222222}"))]));
        assert_eq!((&after.fx[&key(LFX)], &after.fx[&key(GFX)]), (&s(PRE), &s(POST)));
        assert!(!after.fx.contains_key(&key(SFX)) && !after.fx.contains_key(&key(MODES_SFX)));
        // a driver with LFX and an SFX is not "only LFX/GFX"
        let mixed = with_fx(&[(LFX, s("{11111111-1111-1111-1111-111111111111}")), (SFX, s("{22222222-2222-2222-2222-222222222222}"))]);
        assert_eq!(pick_mode(&mixed), Mode::SfxEfx);
        let multi = with_fx(&[(LFX, s("{1}")), (MULTI_SLOTS[0], s("{2}"))]);
        assert_eq!(pick_mode(&multi), Mode::SfxEfx);
        // a device with no effect values at all
        assert_eq!(pick_mode(&DeviceRegState::default()), Mode::SfxEfx);
    }

    #[test]
    fn a_device_without_sfx_falls_back_to_lfx_as_pre_mix_child() {
        let st = with_fx(&[(LFX, s("{11111111-1111-1111-1111-111111111111}")), (MFX, s("{33333333-3333-3333-3333-333333333333}"))]);
        let after = install(st);
        let child = after.child.expect("record");
        assert_eq!(child[&key("PreMixChild")], s("{11111111-1111-1111-1111-111111111111}"), "SFX and EFX absent: LFX");
        assert_eq!(child[&key("PostMixChild")], s(""), "GFX absent too");
    }

    #[test]
    fn already_attached_devices_are_left_alone() {
        let installed = install(realtek_before());
        assert!(plan_install(&installed).unwrap().is_noop(), "installed: nothing to do");

        // ,5 is already Equalizer APO but there is no Child APOs key: an orphan
        let mut orphan = realtek_before();
        orphan.fx.insert(key(SFX), s(&PRE.to_lowercase()));
        assert_eq!(classify(&orphan), Attach::Orphan, "class ids compare case-insensitively");
        assert!(plan_install(&orphan).unwrap().is_noop(), "installing again would record APO as its own child");
        assert_eq!(plan_uninstall(&orphan), Err(PlanError::NoRecord), "the originals are unknown: refuse, change nothing");
        assert!(PlanError::NoRecord.to_string().contains("desinstalador"));

        // another version's record
        let mut v3 = installed.clone();
        v3.child.as_mut().unwrap().insert(key("Version"), s("3"));
        assert_eq!(classify(&v3), Attach::Unsupported);
        assert_eq!(plan_install(&v3), Err(PlanError::UnsupportedRecord));
        assert_eq!(plan_uninstall(&v3), Err(PlanError::UnsupportedRecord));
        v3.child.as_mut().unwrap().remove(&key("Version")); // version 1 has no Version value
        assert_eq!(classify(&v3), Attach::Unsupported);
    }

    #[test]
    fn uninstall_of_a_free_device_is_a_noop_or_drops_a_stale_record() {
        assert!(plan_uninstall(&realtek_before()).unwrap().is_noop());
        let mut stale = realtek_before();
        stale.child = Some(values(&[("Version", s("2"))]));
        assert_eq!(plan_uninstall(&stale).unwrap().ops, vec![Op::DeleteChildKey]);
        let mut done = stale.clone();
        plan_uninstall(&stale).unwrap().simulate(&mut done);
        assert_eq!(done, realtek_before());
    }

    #[test]
    fn non_string_slot_values_are_refused_before_anything_is_planned() {
        let mut st = realtek_before();
        st.fx.insert(key(SFX), RegValue::Dword(7));
        assert_eq!(plan_install(&st), Err(PlanError::UnexpectedValue(SFX.to_string())));
        assert!(PlanError::UnexpectedValue(String::new()).to_string().contains("não entende"));
    }

    #[test]
    fn install_writes_the_record_before_it_touches_the_effect_slots() {
        let plan = plan_install(&DeviceRegState::default()).unwrap();
        let first_fx = plan.ops.iter().position(|o| matches!(o, Op::CreateFxKey | Op::SetFx(..) | Op::DeleteFx(_))).unwrap();
        let last_child = plan.ops.iter().rposition(|o| matches!(o, Op::SetChild(..))).unwrap();
        assert!(last_child < first_fx, "{:?}", plan.ops);
        assert_eq!(plan.ops[0], Op::CreateChildKey);
        assert_eq!(plan.ops[plan.ops.len() - 1].clone(), Op::SetFx(MODES_EFX.into(), modes()));
        // and the Version value (the "done" marker DeviceSelector checks) is the last of the record
        assert_eq!(plan.ops[last_child], Op::SetChild("Version".into(), s("2")));
        // uninstall: slots first, the record last
        let installed = install(DeviceRegState::default());
        let un = plan_uninstall(&installed).unwrap();
        assert_eq!(un.ops, vec![Op::DeleteFxKey, Op::DeleteChildKey], "`!KEY`: the whole key goes");
        let un = plan_uninstall(&install(realtek_before())).unwrap();
        assert_eq!(un.ops.last(), Some(&Op::DeleteChildKey));
    }

    #[test]
    fn uninstall_deletes_a_slot_the_record_has_no_original_for() {
        let mut installed = install(realtek_before());
        // a record whose SFX entry is empty: "no child", and the live value is still APO
        installed.child.as_mut().unwrap().insert(key(SFX), s(""));
        let un = plan_uninstall(&installed).unwrap();
        assert!(un.ops.contains(&Op::DeleteFx(SFX.into())), "{:?}", un.ops);
        // an unrelated value is never overwritten or deleted
        let mut changed = install(realtek_before());
        changed.fx.insert(key(MFX), s("{99999999-9999-9999-9999-999999999999}"));
        let un = plan_uninstall(&changed).unwrap();
        assert!(un.ops.contains(&Op::SetFx(MFX.into(), s(REALTEK_MFX))), "the recorded original comes back: {:?}", un.ops);
        // modes the driver already had are not Clarim's to delete, and a changed value is left alone
        let mut custom = install(realtek_before());
        custom.fx.insert(key(MODES_EFX), RegValue::MultiStr(vec!["{12345678-1234-1234-1234-123456789012}".into()]));
        let un = plan_uninstall(&custom).unwrap();
        assert!(!un.ops.iter().any(|o| matches!(o, Op::DeleteFx(n) if n == MODES_EFX)), "{:?}", un.ops);
    }

    #[test]
    fn restore_undoes_an_install_cut_off_at_any_step() {
        for before in [realtek_before(), bt_before(), DeviceRegState::default()] {
            let plan = plan_install(&before).unwrap();
            for done in 0..=plan.ops.len() {
                let mut now = before.clone();
                Plan { ops: plan.ops[..done].to_vec() }.simulate(&mut now);
                let back = plan_restore(&before, &now);
                back.simulate(&mut now);
                assert_eq!(now, before, "cut after {done} of {} ops", plan.ops.len());
            }
        }
        assert!(plan_restore(&realtek_before(), &realtek_before()).is_noop());
    }

    #[test]
    fn restore_undoes_an_uninstall_cut_off_at_any_step() {
        for before in [realtek_before(), DeviceRegState::default()] {
            let installed = install(before);
            let plan = plan_uninstall(&installed).unwrap();
            for done in 0..=plan.ops.len() {
                let mut now = installed.clone();
                Plan { ops: plan.ops[..done].to_vec() }.simulate(&mut now);
                plan_restore(&installed, &now).simulate(&mut now);
                assert_eq!(now, installed, "cut after {done} of {} ops", plan.ops.len());
            }
        }
    }

    #[test]
    fn registry_values_round_trip_through_their_wire_form() {
        for v in [
            s("{EACD2258-FCAC-4FF4-B36D-419E924A6D79}"),
            s(""),
            s("Áudio"),
            RegValue::MultiStr(vec![MODE.to_string()]),
            RegValue::MultiStr(vec!["a".into(), "bc".into()]),
            RegValue::MultiStr(vec![]),
            RegValue::Dword(0xDEAD_BEEF),
            RegValue::Binary(vec![1, 2, 3]),
            RegValue::Raw(11, vec![9, 9]),
        ] {
            let (ty, bytes) = v.encode();
            assert_eq!(RegValue::decode(ty, &bytes), v, "{v:?}");
        }
        // REG_SZ data is NUL-terminated on the wire and may carry junk after the first NUL
        let (ty, mut bytes) = s("abc").encode();
        assert_eq!(bytes, [b'a', 0, b'b', 0, b'c', 0, 0, 0]);
        bytes.extend_from_slice(&[b'z', 0, 0, 0]);
        assert_eq!(RegValue::decode(ty, &bytes), s("abc"));
        assert_eq!(RegValue::decode(REG_DWORD, &[1, 2]), RegValue::Raw(REG_DWORD, vec![1, 2]), "a short DWORD is kept raw");
        assert_eq!(RegValue::Dword(1).encode(), (REG_DWORD, vec![1, 0, 0, 0]));
    }

    #[test]
    fn the_tracked_names_cover_everything_a_plan_reads() {
        let names = tracked_fx_names();
        for n in SLOTS.iter().chain(&MULTI_SLOTS).chain(&[MODES_SFX, MODES_MFX, MODES_EFX, FX_TITLE, SYSFX]) {
            assert!(names.contains(n), "{n}");
            assert_eq!(*n, n.to_ascii_lowercase(), "names are kept lower case");
        }
    }
}
