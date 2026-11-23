//! Per-app default audio endpoint (the "App volume and device preferences" setting) through the
//! undocumented `Windows.Media.Internal.AudioPolicyConfig` activation factory.
//!
//! windows-rs' generic `RoGetActivationFactory` always asks for `T::IID`, so the raw `combase.dll`
//! export is imported here and the interface is driven through its vtable. Layout for the IID used
//! from build 21390 on (Windows 11 and 21H2): IUnknown 0-2, IInspectable 3-5, nineteen unused slots
//! 6-24, then `SetPersistedDefaultAudioEndpoint` = 25, `GetPersistedDefaultAudioEndpoint` = 26,
//! `ClearAllPersistedApplicationDefaultEndpoints` = 27. Get was verified live on Windows 11 build
//! 22631 (see the `live_get` test); Set and Clear are deliberately never exercised by tests. Older
//! builds use another IID whose vtable layout is NOT verified (sources disagree) and a wrong slot
//! could crash the process, so `Policy::new` refuses them and routing runs volume-only there.
//!
//! Everything that touches the factory must stay on the thread that created it (COM, MTA).

use std::ffi::c_void;
use std::mem::{size_of, transmute_copy};
use std::ptr;

use anyhow::{anyhow, Result};
use windows::core::{GUID, HRESULT, HSTRING};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

#[link(name = "combase", kind = "raw-dylib")]
extern "system" {
    fn RoGetActivationFactory(class_id: *mut c_void, iid: *const GUID, factory: *mut *mut c_void) -> HRESULT;
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
}

const CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";
const IID_21H2: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
/// First Windows build that uses the 21H2 interface (Windows 11 is 22000+).
const FIRST_21H2_BUILD: u32 = 21390;

const SLOT_RELEASE: usize = 2;
const SLOT_SET: usize = 25;
const SLOT_GET: usize = 26;
const SLOT_CLEAR: usize = 27;

const FLOW_RENDER: i32 = 0;
pub(crate) const ROLE_CONSOLE: i32 = 0;
pub(crate) const ROLE_MULTIMEDIA: i32 = 1;
pub(crate) const ROLE_COMMUNICATIONS: i32 = 2;
const ROLES: [i32; 3] = [ROLE_CONSOLE, ROLE_MULTIMEDIA, ROLE_COMMUNICATIONS];

const DEVICE_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";
const RENDER_SUFFIX: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";

type SetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut c_void) -> HRESULT;
type GetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut HSTRING) -> HRESULT;
type ClearFn = unsafe extern "system" fn(*mut c_void) -> HRESULT;
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

/// `IMMDevice::GetId()` string -> the string the policy API wants for a render endpoint.
pub(crate) fn wrap_render_id(endpoint_id: &str) -> String {
    format!("{DEVICE_PREFIX}{endpoint_id}{RENDER_SUFFIX}")
}

/// Inverse of [`wrap_render_id`]; `None` if `s` is not a wrapped render endpoint id.
pub(crate) fn unwrap_render_id(s: &str) -> Option<&str> {
    let rest = strip_prefix_ci(s, DEVICE_PREFIX)?;
    let id = rest.get(..rest.len().checked_sub(RENDER_SUFFIX.len())?)?;
    let suffix = &rest[id.len()..];
    (suffix.eq_ignore_ascii_case(RENDER_SUFFIX) && !id.is_empty()).then_some(id)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// Only the 21H2 interface layout is known; anything older is refused.
fn supported_build(build: u32) -> bool {
    build >= FIRST_21H2_BUILD
}

fn os_build() -> Result<u32> {
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // SAFETY: `info` is a valid OSVERSIONINFOW with its size field set; RtlGetVersion only writes into it.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status < 0 {
        return Err(anyhow!("RtlGetVersion failed (NTSTATUS {status:#x})"));
    }
    Ok(info.dwBuildNumber)
}

/// Borrowed raw view of an HSTRING for an in-parameter; the callee does not take ownership.
fn hstring_ref(h: &HSTRING) -> *mut c_void {
    // SAFETY: HSTRING is repr(transparent) over a pointer, so a bitwise copy is the ABI value. It is
    // not dropped (no ownership taken), and the caller keeps `h` alive across the call.
    unsafe { transmute_copy(h) }
}

/// Reads vtable entry `i` of a COM object as a function pointer of type `F`.
///
/// # Safety
/// `obj` must be a live COM object whose vtable has at least `i + 1` entries and entry `i` must
/// really have the signature `F`.
unsafe fn slot<F: Copy>(obj: *mut c_void, i: usize) -> F {
    let vtbl = *(obj as *const *const *const c_void);
    transmute_copy(&*vtbl.add(i))
}

/// The AudioPolicyConfig activation factory. One reference, released on drop. Not `Send`.
pub(crate) struct Policy {
    raw: *mut c_void,
}

impl Policy {
    pub(crate) fn new() -> Result<Self> {
        let build = os_build()?;
        if !supported_build(build) {
            return Err(anyhow!(
                "Windows build {build} predates {FIRST_21H2_BUILD}; the AudioPolicyConfig vtable layout there is unverified"
            ));
        }
        let class = HSTRING::from(CLASS);
        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: `class` outlives the call, `IID_21H2` and `raw` are valid for the call; on success the
        // callee stores an owned (AddRef'd) interface pointer in `raw`.
        let hr = unsafe { RoGetActivationFactory(hstring_ref(&class), &IID_21H2, &mut raw) };
        hr.ok().map_err(|e| anyhow!("RoGetActivationFactory({CLASS}): {e}"))?;
        if raw.is_null() {
            return Err(anyhow!("RoGetActivationFactory({CLASS}) returned a null factory"));
        }
        Ok(Self { raw })
    }

    /// Persisted default render endpoint of `pid` for `role`, as an `IMMDevice::GetId()` string.
    /// `None` = no per-app rule (the app follows the Windows default).
    pub(crate) fn get(&self, pid: u32, role: i32) -> Result<Option<String>> {
        let mut out = HSTRING::new();
        // SAFETY: `self.raw` is a live factory; slot 26 is GetPersistedDefaultAudioEndpoint(this, pid,
        // flow, role, HSTRING*) per the module docs; HSTRING is repr(transparent) over a pointer, so
        // `&mut out` is a valid out-pointer, and on success `out` owns the returned string (freed on drop).
        let hr = unsafe {
            let f: GetFn = slot(self.raw, SLOT_GET);
            f(self.raw, pid, FLOW_RENDER, role, &mut out)
        };
        hr.ok().map_err(|e| anyhow!("GetPersistedDefaultAudioEndpoint(pid {pid}, role {role}): {e}"))?;
        if out.is_empty() {
            return Ok(None);
        }
        let s = out.to_string_lossy();
        match unwrap_render_id(&s) {
            Some(id) => Ok(Some(id.to_owned())),
            None => Err(anyhow!("unexpected persisted endpoint string {s:?} for pid {pid}")),
        }
    }

    /// Point `pid` at `endpoint` (an `IMMDevice::GetId()` string of a PRESENT render endpoint) for
    /// all three roles; `None` clears the rule. Tries every role, reports the first failure.
    pub(crate) fn set_all_roles(&self, pid: u32, endpoint: Option<&str>) -> Result<()> {
        let wrapped = endpoint.map(|id| HSTRING::from(wrap_render_id(id)));
        let dev = wrapped.as_ref().map_or(ptr::null_mut(), hstring_ref);
        let mut first_err = None;
        for role in ROLES {
            // SAFETY: `self.raw` is a live factory; slot 25 is SetPersistedDefaultAudioEndpoint(this,
            // pid, flow, role, HSTRING) per the module docs. `dev` is null (clear) or borrows
            // `wrapped`, which lives until the end of this function.
            let hr = unsafe {
                let f: SetFn = slot(self.raw, SLOT_SET);
                f(self.raw, pid, FLOW_RENDER, role, dev)
            };
            if let Err(e) = hr.ok() {
                first_err.get_or_insert_with(|| anyhow!("SetPersistedDefaultAudioEndpoint(pid {pid}, role {role}): {e}"));
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Remove every per-app rule of the current user.
    pub(crate) fn clear_all(&self) -> Result<()> {
        // SAFETY: `self.raw` is a live factory; slot 27 is ClearAllPersistedApplicationDefaultEndpoints(this).
        let hr = unsafe {
            let f: ClearFn = slot(self.raw, SLOT_CLEAR);
            f(self.raw)
        };
        hr.ok().map_err(|e| anyhow!("ClearAllPersistedApplicationDefaultEndpoints: {e}"))
    }
}

impl Drop for Policy {
    fn drop(&mut self) {
        // SAFETY: we own exactly one reference from RoGetActivationFactory; slot 2 is IUnknown::Release.
        unsafe {
            let release: ReleaseFn = slot(self.raw, SLOT_RELEASE);
            release(self.raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{enumerate, device_enumerator, ComGuard};
    use std::collections::HashMap;

    const ID: &str = "{0.0.0.00000000}.{549fa0af-7f2b-4e41-8279-fddca9fc370b}";

    #[test]
    fn wrap_unwrap_round_trip() {
        let wrapped = wrap_render_id(ID);
        assert_eq!(
            wrapped,
            r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{549fa0af-7f2b-4e41-8279-fddca9fc370b}#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
        assert_eq!(unwrap_render_id(&wrapped), Some(ID));
    }

    #[test]
    fn unwrap_is_case_insensitive_and_strict() {
        let upper = wrap_render_id(ID).to_uppercase();
        assert_eq!(unwrap_render_id(&upper), Some(ID.to_uppercase().as_str()));
        // capture suffix, missing prefix, empty id, too short, multibyte boundary
        assert_eq!(unwrap_render_id(&wrap_render_id(ID).replace("e6327cad-dcec-4949-ae8a-991e976a79d2", "2eef81be-33fa-4800-9670-1cd474972c3f")), None);
        assert_eq!(unwrap_render_id(ID), None);
        assert_eq!(unwrap_render_id(&wrap_render_id("")), None);
        assert_eq!(unwrap_render_id(r"\\?\SWD#MMDEVAPI#"), None);
        assert_eq!(unwrap_render_id("ééééééééééééééééééééééééééééééééééééééééééééééééééééééé"), None);
    }

    #[test]
    fn only_21h2_and_newer_builds_are_supported() {
        assert!(supported_build(22631));
        assert!(supported_build(FIRST_21H2_BUILD));
        assert!(!supported_build(FIRST_21H2_BUILD - 1));
        assert!(!supported_build(19045));
        assert_eq!(IID_21H2.to_u128(), 0xab3d4648_e242_459f_b02f_541c70306324);
    }

    /// Read-only proof that the IID and vtable slot 26 are right on this machine: the factory is
    /// created and Get is called for every role for every pid that has an audio session.
    /// (Windows answers E_INVALIDARG for a pid without one, e.g. this test process.)
    /// Never calls Set/Clear. Run with `rtk proxy cargo test -p clarim-routing live_get -- --nocapture` to see output.
    #[test]
    fn live_get_works_for_session_pids() {
        let _com = ComGuard::init().expect("com");
        let policy = Policy::new().expect("policy factory");
        println!("os build {}", os_build().expect("build"));
        let own = std::process::id();
        let en = device_enumerator().expect("enumerator");
        let scan = enumerate(&en, own, &mut HashMap::new()).expect("enumerate");
        println!("{} active render endpoints, {} sessions", scan.endpoints.len(), scan.sessions.len());
        for s in &scan.sessions {
            let i = &s.info;
            for role in ROLES {
                match policy.get(i.pid, role) {
                    Ok(r) => println!("pid {} {} role {role} (session on {}): {r:?}", i.pid, i.exe, i.device_id),
                    Err(e) => panic!("get failed for pid {} {}: {e}", i.pid, i.exe),
                }
            }
        }
    }
}
