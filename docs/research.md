# Clarim research notes (Haiku sweep, 2026-11-21)

Machine-generated research. Facts marked low confidence must be verified against crate source in ~/.cargo/registry before relying on them. Code from GPL sources was NOT copied.


## routing

### Summary

Research on per-app audio output routing and per-app volume/mute on Windows 10/11 from Rust. Core findings: (1) Per-app output routing uses the undocumented WinRT class "Windows.Media.Internal.AudioPolicyConfig" (activated via RoGetActivationFactory), which exposes SetPersistedDefaultAudioEndpoint / GetPersistedDefaultAudioEndpoint / ClearAllPersistedApplicationDefaultEndpoints. EarTrumpet (MIT, with a carve-out preamble for three named entities) and Earmark (MIT) both implement it and agree on the IIDs for Windows 11: ab3d4648-e242-459f-b02f-541c70306324. For pre-21H2 Windows 10 the IID is 2a59116d-6c4f-45e0-a74f-707e3fef9258. The vtable is IUnknown (3 slots) + IInspectable (3 slots) + reserved/placeholder slots + Set, Get, Clear. Set is at slot 25, Get at 26, Clear at 27 for the Win11 layout (both sources agree). The Win10 layout is UNRESOLVED: EarTrumpet has 19 placeholders (Set at 25) while Earmark has 13 (Set at 19). The device-id string is "\\?\SWD#MMDEVAPI#" + IMMDevice.GetId() (e.g. "{0.0.0.00000000}.{guid}") + "#{e6327cad-dcec-4949-ae8a-991e976a79d2}" for render or "#{2eef81be-33fa-4800-9670-1cd474972c3f}" for capture. EarTrumpet sets eMultimedia and eConsole (not eCommunications). A null/empty deviceId clears the rule. Live effect: EarTrumpet forces live sessions to move with the same call, so it applies to running streams (medium confidence). (2) Rust 0.58: windows-rs RoGetActivationFactory is generic over T: Interface and uses T::IID, so a custom IID needs a raw extern against combase.dll (as EarTrumpet does). Features: Win32_Media_Audio (IAudioSessionManager2, IAudioSessionControl2, ISimpleAudioVolume, IAudioSessionEnumerator, eRender/eConsole), Win32_System_Com, Win32_System_Threading (OpenProcess, QueryFullProcessImageNameW), Win32_Foundation. HSTRING lives in windows-core, no feature needed. (3) Session enumeration: IMMDeviceEnumerator -> GetDefaultAudioEndpoint (or per device) -> Activate IAudioSessionManager2 -> GetSessionEnumerator -> IAudioSessionControl -> cast to IAudioSessionControl2 (GetProcessId, IsSystemSoundsSession) and ISimpleAudioVolume (SetMasterVolume(float, LPCGUID EventContext), SetMute). AUDCLNT_S_NO_SINGLE_PROCESS (0x889000D) is a success code. (4) Gotchas: Chromium/Electron audio runs in a separate audio-service process, so the session PID may not be the window PID; UWP and system-sounds sessions need special handling; elevated processes may refuse OpenProcess. Versions: wasapi 0.15.0 and windows 0.58.0 both exist on crates.io, but the current stable releases are much newer (wasapi 0.27.0, windows 0.62.2, ringbuf 0.6.0, nnnoiseless 0.5.2).

### Facts

- [high] EarTrumpet defines IAudioPolicyConfigFactoryVariantFor21H2 with GUID ab3d4648-e242-459f-b02f-541c70306324 (InterfaceIsIInspectable), with 19 __incomplete__ placeholder methods (add/remove_CtxVolumeChange, add/remove_RingerVibrateStateChange(d), Set/GetVolumeGroupGainForId, GetActiveVolumeGroupForEndpointId, GetVolumeGroupsForEndpoint, GetCurrentVolumeContext, Set/GetVolumeGroupMuteForId, Set/GetRingerVibrateState, Set/Reset/GetPreferredChatApplication, GetCurrentChatApplications, add/remove_ChatContextChanged) followed by SetPersistedDefaultAudioEndpoint, GetPersistedDefaultAudioEndpoint, ClearAllPersistedApplicationDefaultEndpoints. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/Interop/MMDeviceAPI/IAudioPolicyConfigFactoryVariantFor21H2.cs)
- [high] EarTrumpet's downlevel variant uses GUID 2a59116d-6c4f-45e0-a74f-707e3fef9258 with the same method names and the same 19 placeholder slots before Set/Get/Clear. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/Interop/MMDeviceAPI/IAudioPolicyConfigFactoryVariantForDownlevel.cs)
- [high] Earmark (MIT) uses the same Win11 IID ab3d4648-e242-459f-b02f-541c70306324 and the Win10 IID 2a59116d-6c4f-45e0-a74f-707e3fef9258. Its Win11 vtable is GetIids, GetRuntimeClassName, GetTrustLevel, then 19 __reserved slots, then Set/Get/Clear. Its Win10 vtable has only 13 reserved slots before Set/Get/Clear. This conflicts with EarTrumpet's 19 and is unresolved. (https://github.com/hoobio/earmark/blob/HEAD/src/Earmark.Audio/Interop/IAudioPolicyConfigFactory.cs)
- [medium] Vtable slot numbering (0-based): IUnknown 0-2, IInspectable 3-5, 19 placeholders 6-24, so Set=25, Get=26, Clear=27 for the Win11 layout. This is derived from the source and matches Earmark's Win11 layout. The Win10 numbers differ between sources. (https://github.com/hoobio/earmark/blob/HEAD/src/Earmark.Audio/Interop/IAudioPolicyConfigFactory.cs)
- [high] Set signature: SetPersistedDefaultAudioEndpoint(uint processId, EDataFlow flow, ERole role, HSTRING deviceId) returns HRESULT. Get signature: GetPersistedDefaultAudioEndpoint(uint processId, EDataFlow flow, ERole role, out HSTRING deviceId). Both are [PreserveSig]. EDataFlow: eRender=0, eCapture=1, eAll=2. ERole: eConsole=0, eMultimedia=1, eCommunications=2. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/Interop/MMDeviceAPI/IAudioPolicyConfigFactory.cs)
- [high] Factory selection in EarTrumpet: if Environment.OSVersion.Build >= 21390 (OSVersions.Version21H2) use the 21H2 variant, else the downlevel variant. Activation is RoGetActivationFactory("Windows.Media.Internal.AudioPolicyConfig", iid) via combase.dll. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/Interop/Helpers/AudioPolicyConfigFactory.cs)
- [high] Earmark selects the Win11 IID when build >= 22000 and otherwise the Win10 IID. It tries class names Windows.Media.Internal.AudioPolicyConfig and then Windows.Media.AudioPolicyConfig. The two thresholds (21390 in EarTrumpet, 22000 in Earmark) only differ on pre-release builds 21390-21999. (https://github.com/hoobio/earmark/blob/HEAD/src/Earmark.Audio/Services/AudioPolicyService.cs)
- [high] Device-id string: MMDEVAPI_TOKEN = "\\?\SWD#MMDEVAPI#" (verbatim C# literal). Render suffix "#{e6327cad-dcec-4949-ae8a-991e976a79d2}", capture suffix "#{2eef81be-33fa-4800-9670-1cd474972c3f}". EarTrumpet's GenerateDeviceId is token + endpoint id + suffix, and UnpackDeviceId strips them on read. Earmark's BuildPersistedId uses the same format. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/DataModel/WindowsAudio/Internal/AudioPolicyConfigService.cs)
- [high] The endpoint id in EarTrumpet is IMMDevice.GetId(), which is the string format {0.0.0.00000000}.{guid}, read in AudioDevice's constructor (_id = device.GetId()). (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/DataModel/WindowsAudio/Internal/AudioDevice.cs)
- [high] EarTrumpet's SetDefaultEndPoint calls SetPersistedDefaultAudioEndpoint twice, first with ERole.eMultimedia and then with ERole.eConsole. It does not set eCommunications. When the device string is empty, it passes IntPtr.Zero (hstring = 0) to clear. EarTrumpet guards the call with TryFind(id), because an invalid device id can silence the app. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/DataModel/WindowsAudio/Internal/AudioPolicyConfigService.cs)
- [high] EarTrumpet's GetDefaultEndPoint passes ERole.eMultimedia | ERole.eConsole, which equals 1 (eMultimedia), since eConsole=0. It also gates the read on Windows build >= RS4 (17134), and returns null otherwise. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/DataModel/WindowsAudio/Internal/AudioPolicyConfigService.cs)
- [high] Earmark's ApplyToRoles 'All' scope sets eConsole, eMultimedia and eCommunications. Its Set call first reads the current persisted value and skips the write if it already matches (idempotent). (https://github.com/hoobio/earmark/blob/HEAD/src/Earmark.Audio/Services/AudioPolicyService.cs)
- [medium] EarTrumpet's AudioDeviceSession re-applies the persisted endpoint to a live session via MoveToDevice (which calls SetDefaultEndPoint with the session's PID). It then waits 200 ms, because the session can flip Active/Inactive during the move. This suggests the call moves already-running sessions, not only future streams. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/DataModel/WindowsAudio/Internal/AudioDeviceSession.cs)
- [low] A third-party tool (winappaudiorouter) reports that it uses the same routing path as EarTrumpet and SoundVolumeView (AudioPolicyConfig::SetPersistedDefaultAudioEndpoint). It routes by process name (e.g. chrome.exe), and per-app routing needs an active audio session for that app. Some apps need a playback restart because session rebinding is asynchronous. (https://pypi.org/project/winappaudiorouter/1.1.0/)
- [medium] Microsoft documents per-app output selection only through Settings > App volume and device preferences (Windows 10). It does not document the SetPersistedDefaultAudioEndpoint call, which is undocumented. (https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/default-audio-endpoint-selection)
- [high] IAudioSessionControl2 exposes GetProcessId, GetSessionIdentifier, GetSessionInstanceIdentifier, IsSystemSoundsSession and SetDuckingPreference. The object must be used on a COM-initialized thread, and it must be released on the same thread that obtained it via GetService. (https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nn-audiopolicy-iaudiosessioncontrol2)
- [high] EarTrumpet's ReadProcessId treats both S_OK and AUDCLNT_S_NO_SINGLE_PROCESS as success. AUDCLNT_S_NO_SINGLE_PROCESS = 0x889000D (from EarTrumpet HRESULT.cs), and the returned PID is the session's originating process. For system-sounds sessions EarTrumpet maps PID to 0, because Windows 10 can report taskhostw.exe as the PID. (https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/DataModel/WindowsAudio/Internal/AudioDeviceSession.cs)
- [high] ISimpleAudioVolume::SetMasterVolume(float fLevel, LPCGUID EventContext): fLevel is in [0.0, 1.0]. E_INVALIDARG is returned if out of range. An event is generated only when the level actually changes. A NULL EventContext makes the client's notification receive a NULL context. AUDCLNT_E_DEVICE_INVALIDATED and AUDCLNT_E_SERVICE_NOT_RUNNING are possible. (https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-isimpleaudiovolume-setmastervolume)
- [high] windows-rs 0.58.0 Win32/System/Threading exports OpenProcess(dwdesiredaccess, binherithandle, dwprocessid) and QueryFullProcessImageNameW(hprocess, dwflags: PROCESS_NAME_FORMAT, lpexename: PWSTR, lpdwsize: *mut u32). The feature is therefore Win32_System_Threading. (https://github.com/microsoft/windows-rs/blob/0.58.0/crates/libs/windows/src/Windows/Win32/System/Threading/mod.rs)
- [high] windows-rs 0.58.0 Win32/System/WinRT exports RoGetActivationFactory<T: Interface>(&HSTRING) -> Result<T>, which uses T::IID internally. It also exports WindowsCreateString. The feature is Win32_System_WinRT. The generated code passes the HSTRING with core::mem::transmute_copy. (https://github.com/microsoft/windows-rs/blob/0.58.0/crates/libs/windows/src/Windows/Win32/System/WinRT/mod.rs)
- [high] windows-rs 0.58.0 Win32/Media/Audio contains the IAudioSessionControl2, IAudioSessionManager2, IAudioSessionEnumerator and ISimpleAudioVolume bindings. The feature is Win32_Media_Audio. (https://github.com/microsoft/windows-rs/blob/0.58.0/crates/libs/windows/src/Windows/Win32/Media/Audio/mod.rs)
- [high] Version check on crates.io: wasapi 0.15.0 (published 2024-06-17) and windows 0.58.0 (published 2024-07-03) both exist. Current max_stable_version values: wasapi 0.27.0, windows 0.62.2, ringbuf 0.6.0, nnnoiseless 0.5.2, windows-core 0.100.0 (as returned, not cross-checked). (https://crates.io/api/v1/crates/wasapi)
- [high] Earmark's ADR states that the audio endpoint id is not stable across a driver reinstall or OS update. Earmark keys persisted per-device state by DEVPKEY_Device_ContainerId plus flow, which is stable across reinstall and reboot. (https://github.com/hoobio/earmark/blob/HEAD/docs/adr/device-persistence-and-identity.md)
- [medium] Chromium's audio service runs as its own utility process on Windows, so the audio session's PID may not be the PID of the window's process. Chromium resolves the main process as the oldest ancestor sharing the same exe image. (https://chromium.googlesource.com/chromium/src/+/main/content/public/browser/audio_service_info.h)
- [high] EarTrumpet's LICENSE is MIT with a preamble excluding three entities (Yellow Elephant Productions, Tidal Media Inc., Articent Group LLC). Earmark's LICENSE is plain MIT. (https://github.com/File-New-Project/EarTrumpet/blob/master/LICENSE)
- [medium] The com-policy-config Rust crate (docs.rs 0.6.0) covers only the legacy IPolicyConfig interface, with IID f8679f50-850a-41cf-9c72-430f290290c8 and CLSID 870af99c-171d-4f9e-af0d-e63df40c2bc9 (PolicyConfigClient). It does not cover AudioPolicyConfig. EarTrumpet's SetDefaultDevice uses the legacy IPolicyConfig path (global default), not AudioPolicyConfig. (https://docs.rs/com-policy-config/0.6.0/src/com_policy_config/lib.rs.html)

### Gotchas

- UNRESOLVED vtable conflict for Win10 (pre-21H2): EarTrumpet's downlevel interface has 19 placeholder slots (Set at 25) while Earmark's Win10 interface has 13 (Set at 19). Using the wrong count calls a wrong method silently. Verify on a real build before relying on the Win10 path.
- The OS threshold differs: EarTrumpet uses build >= 21390 for the 21H2 variant, Earmark uses >= 22000 for Win11. They agree on release builds (Win10 21H2 = 19044 uses the Win10 IID in both, Win11 22000+ uses the Win11 IID in both), but any code should use one shared rule.
- No evidence found either way for a changed IID on Windows 11 24H2/25H2. The search was inconclusive, so treat the IID as unverified on those builds until tested.
- EarTrumpet's read path passes ERole.eMultimedia | ERole.eConsole = 1, which only reads eMultimedia. Read eMultimedia explicitly rather than copying this.
- The device string must be built from the exact IMMDevice.GetId() output (not the friendly name). The endpoint id changes on driver reinstall or OS update (Earmark ADR), so persisted rules can silently stop matching. Earmark keys devices by DEVPKEY_Device_ContainerId plus flow instead.
- Pass a valid, currently present device id. EarTrumpet guards with TryFind because an invalid id can leave an app silent. Passing a null/empty hstring clears the rule (EarTrumpet passes IntPtr.Zero).
- Set the eMultimedia and eConsole roles explicitly; eCommunications is not set by EarTrumpet. Earmark's 'All' scope sets all three. Choose deliberately for Meet/Discord, since apps may use the communications role.
- The rule is looked up by process, but whether Windows keys it by PID or by app identity (exe/package) is not documented. Persisted rules survive reboots per third-party reports (low confidence). Test whether a rule survives an app restart.
- Chromium/Electron (Chrome, Discord, Spotify client): audio runs in a separate audio-service utility process on Windows, so the session PID may not be the window's PID. Map sessions to the app by walking the process tree (oldest ancestor with the same exe) or by exe name, not by a single PID.
- Processes with several audio sessions: one exe can own multiple sessions on one endpoint (and one session per endpoint). Enumerate per render endpoint and group by PID or exe, since a process can have several sessions.
- System-sounds session: IsSystemSoundsSession returns S_OK for it, and on some Win10 builds it reports taskhostw.exe's PID. Map that PID to 0 and ignore it for per-app routing, as EarTrumpet does.
- UWP apps: EarTrumpet documents a Windows 10 bug where a UWP app's persisted endpoint is not honored on relaunch (reproduced with Minesweeper). Its workaround is to re-apply the endpoint after the session appears, with a 200 ms delay and checks for Active/Inactive flips. Expect the same pattern. UWP sessions may also be owned by a host process rather than the packaged app's exe (general knowledge, unverified here).
- Elevation: OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) on an elevated or other-integrity process from a standard user will likely fail with access denied (general Win32 behavior, not verified against a source). Degrade gracefully and show the exe name as unknown.
- Threading: call CoInitializeEx (MTA) on the thread that uses these COM objects, and release interfaces on that same thread. IAudioSessionControl2 must be released on the thread that obtained it (Learn docs). Do not share COM objects across threads without marshalling.
- Live effect: the call appears to move already-playing sessions (EarTrumpet's FORCE-MOVE), but moves can race with Active/Inactive transitions. Re-check after a short delay, and expect some apps to need a playback restart (low-medium confidence).
- Undocumented API: may break in a future Windows update. Keep a fallback to the documented Settings-based behavior (App volume and device preferences) and isolate the call behind one module.
- Version note: the user's crate pins are old. wasapi 0.15.0 and windows 0.58.0 exist, but stable is wasapi 0.27.0 and windows 0.62.2. Moving to newer windows crate may change feature names and generic signatures, so verify the snippets against the version actually used.
- License: EarTrumpet is MIT with a carve-out for Yellow Elephant Productions, Tidal Media Inc. and Articent Group LLC. Earmark is plain MIT. The snippets above are original sketches, not copied code.

### Open questions

- Which Win10 (pre-21H2) vtable is correct: 19 placeholders (EarTrumpet, Set at 25) or 13 (Earmark, Set at 19)? Verify by inspecting the factory's vtable on a real Win10 build (for example, calling the slots with a known harmless input and observing HRESULT).
- Do the Win11 24H2 and 25H2 builds still use ab3d4648-e242-459f-b02f-541c70306324 and the same 19 placeholders? Not checked. Check EarTrumpet and Earmark commit history for newer changes, or test on a 24H2/25H2 machine.
- Is the persisted rule keyed by PID, by exe path, or by package identity? This determines whether one call covers all sessions for an app and whether it survives restarts.
- Does SetPersistedDefaultAudioEndpoint move already-running streams for all apps (Chromium, UWP, Electron), or only new streams? EarTrumpet's FORCE-MOVE suggests yes for live sessions, but there is no authoritative source.
- Exact SetMute signature (BOOL bMute, LPCGUID EventContext) was not fetched from Learn. Confirm it.
- In windows-rs 0.58, do IsSystemSoundsSession and CoCreateInstance::<_, T>(&MMDeviceEnumerator, ...) have the return types assumed in the sketch? Confirm against the generated bindings.
- Is the windows-rs 0.58 interface macro (#[interface]) usable for IInspectable-based custom interfaces with placeholder slots? The raw-vtable approach in the sketch avoids this. The macro source was not fetched (the path returned 404).
- Does the eCommunications role matter for Discord and Meet routing in practice? Not tested.
- The crates.io windows-core max_stable_version reported 0.100.0. Verify before pinning, since that looks unusually high.

### Code snippets


**Rust 0.58 sketch: call the undocumented AudioPolicyConfig factory with a raw combase import (no windows-rs generic), build the MMDEVAPI device string, and set the persisted endpoint for eMultimedia then eConsole (as EarTrumpet does). Not compiled; the vtable slot indices are for the Win11 layout only.** — https://github.com/File-New-Project/EarTrumpet/blob/master/EarTrumpet/DataModel/WindowsAudio/Internal/AudioPolicyConfigService.cs (original sketch, written from API facts (EarTrumpet MIT with carve-out; Earmark MIT); no code copied)

```rust
use std::ffi::c_void;
use windows::core::{GUID, HRESULT, HSTRING};

// windows-rs RoGetActivationFactory<T> needs T: Interface and uses T::IID, so use the raw export with our own IID.
#[link(name = "combase")]
extern "system" {
    fn RoGetActivationFactory(class_id: *mut c_void, iid: *const GUID, factory: *mut *mut c_void) -> HRESULT;
}

const CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";
const IID_WIN11: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
// Slots: 0-2 IUnknown, 3-5 IInspectable, 6-24 nineteen placeholders. Win10 layout differs (see gotchas).
const SLOT_SET: usize = 25;

type SetFn = unsafe extern "system" fn(this: *mut c_void, pid: u32, flow: i32, role: i32, dev: *mut c_void) -> HRESULT;

unsafe fn slot<F: Copy>(obj: *mut c_void, i: usize) -> F {
    let vtbl = *(obj as *const *const *const c_void);
    std::mem::transmute_copy(&*vtbl.add(i))
}

/// flow: 0 = eRender, 1 = eCapture. endpoint_id: IMMDevice::GetId(), e.g. "{0.0.0.00000000}.{guid}". None clears the rule.
pub unsafe fn set_app_endpoint(pid: u32, flow: i32, endpoint_id: Option<&str>) -> windows::core::Result<()> {
    let mut factory: *mut c_void = std::ptr::null_mut();
    let class = HSTRING::from(CLASS);
    RoGetActivationFactory(std::mem::transmute_copy(&class), &IID_WIN11, &mut factory).ok()?;
    let suffix = if flow == 0 { "#{e6327cad-dcec-4949-ae8a-991e976a79d2}" } else { "#{2eef81be-33fa-4800-9670-1cd474972c3f}" };
    let dev = endpoint_id.map(|id| HSTRING::from(format!(r"\\?\SWD#MMDEVAPI#{id}{suffix}")));
    let dev_raw: *mut c_void = dev.as_ref().map_or(std::ptr::null_mut(), |h| std::mem::transmute_copy(h));
    let set: SetFn = slot(factory, SLOT_SET);
    for role in [1, 0] { // eMultimedia, then eConsole
        set(factory, pid, flow, role, dev_raw).ok()?;
    }
    // ponytail: factory ref never released; call Release (slot 2) when done.
    Ok(())
}
```

**Rust 0.58 sketch: enumerate audio sessions on the default render endpoint, read PID, system-sounds flag, display name, set volume/mute, and resolve the exe path. Not compiled; method return types (for IsSystemSoundsSession and CoCreateInstance generics) should be checked against windows-rs 0.58.** — https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nn-audiopolicy-iaudiosessioncontrol2 (original sketch, written from Microsoft Learn API docs and windows-rs 0.58 module layout; no code copied)

```rust
use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::*;

pub unsafe fn list_render_sessions() -> windows::core::Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?; // MTA; keep all COM calls on this thread
    let en: IMMDeviceEnumerator = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
    let list = mgr.GetSessionEnumerator()?;
    for i in 0..list.GetCount()? {
        let ctl: IAudioSessionControl = list.GetSession(i)?;
        let ctl2: IAudioSessionControl2 = ctl.cast()?;
        let pid = ctl2.GetProcessId()?; // AUDCLNT_S_NO_SINGLE_PROCESS (0x889000D) is a success code
        let is_system = ctl2.IsSystemSoundsSession() == S_OK;
        let _name: PWSTR = ctl.GetDisplayName()?; // free with CoTaskMemFree
        let vol: ISimpleAudioVolume = ctl.cast()?;
        vol.SetMasterVolume(0.5, std::ptr::null())?;
        vol.SetMute(false, std::ptr::null())?;
        if !is_system && pid != 0 {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)?;
            CloseHandle(h)?;
            // buf[..len as usize] is the full exe path
        }
    }
    Ok(())
}
```

## denoise

### Summary

Research for a Rust Windows 11 real-time mic noise suppressor (48 kHz, 10 ms frames). Recommendation: use DeepFilterNet3 via libDF (git dependency, feature "tract" plus "default-model") as the main denoiser, with nnnoiseless 0.5.2 (RNNoise port) as a light fallback. The crates.io package "deep_filter" 0.2.5 is stale (2022) and does not match the repo, so pin a git revision. Background-voice suppression has no practical off-the-shelf Rust option verified today; the realistic path is VAD gating plus a level/proximity expander calibrated to the user's voice. The project's pinned crate versions (wasapi 0.15, windows 0.58, ringbuf 0.4) are well behind crates.io latest (0.27.0, 0.62.2, 0.6.0); check compatibility before upgrading.

### Facts

- [high] crates.io 'deep_filter' has max version 0.2.5 (published 2022-07-28, license MIT/Apache-2.0, description 'Noise supression using deep filtering'). Its features are bin, cache, dataset, flac, transforms, vorbis, default=[]. It does not match the current repo. (https://crates.io/api/v1/crates/deep_filter)
- [high] The DeepFilterNet repo's libDF/Cargo.toml declares package name 'deep_filter', version 0.5.7-pre, lib name 'df', license MIT/Apache-2.0, rust-version 1.70. This is not on crates.io as 0.5.x, so use a git dependency: deep_filter = { git = "https://github.com/Rikorose/DeepFilterNet", default-features = false, features = ["tract","default-model"] }. Check the exact rev. (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/Cargo.toml)
- [high] libDF features: default = [default-model, vorbis, flac]. default-model embeds DeepFilterNet3_onnx.tar.gz via include_bytes!; default-model-ll embeds DeepFilterNet3_ll_onnx.tar.gz. tract = [transforms, logging, dep:tract-core, tract-onnx, tract-pulse, tract-hir, rust-ini, ndarray, anyhow, flate2, tar]. transforms pulls ndarray and rubato. capi = [tract, default-model, ndarray, logging]. (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/Cargo.toml)
- [high] Pinned deps in libDF: ndarray ^0.15 (serde), tract-core/onnx/pulse/hir ^0.21.4, rust-ini ^0.21, rubato 0.14, realfft ^3.1, rustfft ^6.1, num-complex ^0.4, itertools 0.12, hound 3.4 (wav-utils), lewton/ogg/claxon optional. crates.io latest tract-core is 0.23.10, latest ndarray stable is 0.17.2, so a project using newer ndarray will need the 0.15 pin or a version split. (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/Cargo.toml)
- [high] DfTract API (libDF/src/tract.rs): pub struct DfParams with Default impl (loads the embedded model), DfParams::new(PathBuf) and DfParams::from_bytes(&[u8]). pub struct RuntimeParams with pub fields n_ch, post_filter, post_filter_beta, atten_lim_db, min_db_thresh, max_db_erb_thresh, max_db_df_thresh, reduce_mask. Constructor RuntimeParams::default_with_ch(channels) with defaults: post_filter false, beta 0.02, atten_lim_db 100, min_db_thresh -10, max_db_erb_thresh 30, max_db_df_thresh 20, reduce_mask MEAN. Builders: with_post_filter(beta), with_atten_lim(db), with_thresholds(min,erb,df), with_mask_reduce(ReduceMask). (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/src/tract.rs)
- [high] DfTract::new(dfp: DfParams, rp: &RuntimeParams) -> anyhow::Result<DfTract>. Reads sr and hop_size from the model config.ini (pub fields sr, hop_size, fft_size, lookahead are used by enhance_wav). Realtime frame call: pub fn process(&mut self, noisy: ArrayView2<f32>, enh: ArrayViewMut2<f32>) -> Result<f32>, with shape [channels, hop_size], returning the lsnr value in dB. Also set_atten_lim(db), set_pf_beta(beta), process_raw() for spectral outputs, get_spec_noisy/get_spec_enh. (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/src/tract.rs)
- [high] Usage pattern from libDF's enhance_wav bin: let mut model = DfTract::new(df_params.clone(), &r_params)?; then noisy.axis_chunks_iter(Axis(1), model.hop_size) zipped with enh.axis_chunks_iter_mut(Axis(1), model.hop_size), calling model.process(ns_f, enh_f)? per chunk. Resample to model.sr when the input rate differs. Algorithmic delay = fft_size - hop_size + lookahead*hop_size. (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/src/bin/enhance_wav.rs)
- [medium] atten_lim_db: abs value; >=100 means no limit, <0.01 means no output attenuation; otherwise the gain floor is 10^(-lim/20). set_atten_lim(db) updates it at runtime. post_filter is enabled when post_filter_beta > 0. min_db_thresh, max_db_erb_thresh and max_db_df_thresh gate stage application based on lsnr (apply_stages(lsnr) returns apply_gains, apply_gain_zeros, apply_df). (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/src/tract.rs)
- [medium] The repo's C-ABI (capi.rs) exposes df_create, df_get_frame_length, df_set_atten_lim, df_set_post_filter_beta, df_process_frame(input,output) and df_process_frame_raw. It is a usable binding path for non-Rust hosts. (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/src/capi.rs)
- [high] The README says the DeepFilterNet3 model is used for real-time, with a LADSPA plugin and PipeWire filter-chain integration in ladspa/. That plugin is Linux-only, so a Windows port would need the tract API directly. Versions: DFN1 (2022), DFN2 (2022, DPDFNet follow-up 2025), DFN3 (Interspeech 2023). An LL (low-latency) model variant exists via default-model-ll. (https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/README.md)
- [medium] DeepFilterNet original RTF: 0.19 single-threaded on an i5-8250U notebook CPU (paper arXiv 2305.08227, which says the real-time loop is Rust with tract). DFN2 is cited at RTF 0.42 on a Raspberry Pi 4 (secondary source). No verified tract-based RTF or latency in ms for DFN3 was found. (https://arxiv.org/pdf/2305.08227.pdf)
- [high] nnnoiseless crate: latest 0.5.2 published 2025-12-18, license BSD-3-Clause (crates.io truncated; verify in Cargo.toml). Default features bin, dasp; optional capi, train. Public API: pub use denoise::DenoiseState; DenoiseState::new() -> Box<DenoiseState>; process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32. The returned f32 is the VAD probability for that frame. FRAME_SIZE = 480 samples (10 ms at 48 kHz). (https://raw.githubusercontent.com/jneem/nnnoiseless/master/src/denoise.rs)
- [medium] GTCRN (Rong Xiaobin, ICASSP 2024) is MIT licensed. Official figures: 23.7K params and 39.6 MFLOPs. Streaming RTF 0.07 on an i5-12400. Community ports list about 48K params and 33 MMACs/s, so the figures are inconsistent across versions. (https://github.com/sherryyu33/gtcrn)
- [medium] earshot 1.2.2 (crates.io) is a pure Rust VAD, license MIT OR Apache-2.0. silero_vad_rs 0.1.2 (MIT) is a Rust port of Silero VAD. voice_activity_detector 0.2.1 has a non-standard license and should be avoided. (https://crates.io/api/v1/crates/earshot)
- [high] Crate versions on crates.io (2026-11-21): wasapi max_stable 0.27.0 (project pins 0.15); windows 0.62.2 (project pins 0.58); ringbuf 0.6.0 (project pins 0.4); ndarray 0.17.2; tract-core max 0.23.10. (https://crates.io/api/v1/crates/wasapi)

### Gotchas

- deep_filter on crates.io is 0.2.5 from 2022 and does not contain the current libDF API (no default-model-ll, no DfTract with current RuntimeParams). Use a git dependency pinned to a commit.
- The default-model feature embeds the model tarball with include_bytes! from ../../models; a git checkout must include the models directory or the build fails.
- libDF pins ndarray ^0.15 and tract ^0.21.4. Newer ndarray (0.17) or tract (0.23) will not unify. Keep the audio pipeline's ndarray at 0.15 or isolate DF behind a thin ring-buffer boundary.
- DfTract constructs once per stream and is stateful; do not construct per frame. Check Send/Sync before moving it to a Windows audio thread (not verified).
- Latency for DFN3 = (fft_size - hop) + lookahead*hop; fft_size 960 and hop 480 give 10 ms STFT delay, plus lookahead frames. Verify the lookahead value from the model config.
- The project's wasapi 0.15 / windows 0.58 / ringbuf 0.4 are old; latest on crates.io are 0.27.0 / 0.62.2 / 0.6.0. Upgrade is a separate task with API changes.
- RNNoise/nnnoiseless input scale: the original RNNoise expects samples in 16-bit PCM range (not normalized -1..1). Verify in the nnnoiseless example before feeding float audio.
- Background-voice (other speakers) suppression is not solved by any stationary-noise model like DFN3 or GTCRN; they treat other speech as signal. Need target-speaker extraction or gating.

### Open questions

- Exact DFN3 hop_size and lookahead values from the bundled config.ini (expected 480 hop, 960 FFT; not fetched from the tarball).
- Measured tract RTF and per-frame latency for DFN3 on a Windows desktop CPU: not found in sources. Needs a local benchmark.
- Does DfTract implement Send so it can run on a dedicated thread? Not checked.
- Does the git rev at HEAD still build with rustc 1.96 and tract 0.21.x? Not built here.
- Per-frame CPU cost of nnnoiseless (RNNoise) on Windows: no measurement found.
- Silero VAD LICENSE (snakers4/silero-vad) could not be fetched; the MIT license is from memory (low confidence, verify).
- Target-speaker / personalized background voice models usable from Rust (pDeepFilterNet, etc.): not researched in depth; WebSearch was unavailable for this topic due to a classifier error on the second and third calls, so this section is partially unverified.
- WebRTC APM (BSD-3) noise suppression and VAD as a reference design for the DSP fallback: not checked in this run.

### Code snippets


**Minimal per-frame DeepFilterNet3 use via DfTract (signatures from libDF; loop pattern from enhance_wav.rs). Not compiled in this research.** — https://raw.githubusercontent.com/Rikorose/DeepFilterNet/main/libDF/src/tract.rs (MIT/Apache-2.0)

```rust
use deep_filter::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{Array2, Axis};

fn main() -> anyhow::Result<()> {
    let rp = RuntimeParams::default_with_ch(1)
        .with_atten_lim(12.0)          // limit suppression to 12 dB
        .with_post_filter(0.02);        // optional post-filter beta
    let mut model = DfTract::new(DfParams::default(), &rp)?;
    let hop = model.hop_size;           // expected 480 at 48 kHz (verify from config.ini)
    let mut noisy = Array2::<f32>::zeros((1, hop)); // [channels, hop]
    let mut enh = Array2::<f32>::zeros((1, hop));
    let lsnr = model.process(noisy.view(), enh.view_mut())?;
    println!("lsnr {lsnr} dB, sr {}", model.sr);
    Ok(())
}
```

**nnnoiseless per-frame call signature (RNNoise fallback)** — https://raw.githubusercontent.com/jneem/nnnoiseless/master/src/denoise.rs (BSD-3-Clause (verify))

```rust
use nnnoiseless::DenoiseState;
let mut st = DenoiseState::new();          // Box<DenoiseState>
let mut out = [0f32; 480];
let vad: f32 = st.process_frame(&mut out, &input_480); // returns VAD prob
```

## virtmic_service

### Summary

Research for a Rust Windows 11 audio tool (virtual mic + per-app channels + Windows service agent). Version check first: crates.io shows wasapi 0.27.0 (not 0.15), windows 0.62.2 (not 0.58), ringbuf 0.6.0, nnnoiseless 0.5.2 (matches), windows-service 0.8.1. Virtual mic options: VB-Cable is donationware with no verified redistribution, silent-install, or endpoint-name terms. The VirtualDrivers/Virtual-Audio-Driver project is MIT-licensed and creates a virtual speaker and virtual mic, but its README says it is beta and requires test signing, so it fails the no-test-mode requirement. SteelSeries Sonar exposes per-channel playback endpoints named "SteelSeries Sonar - Game", "- Chat", "- Media" (and "- Aux" when enabled), plus "SteelSeries Sonar - Microphone" for input, per support pages (medium confidence; names change across Sonar versions). Service pattern: WTSQueryUserToken requires the caller to run as LocalSystem with SE_TCB_NAME, header wtsapi32.h, lib Wtsapi32.lib, minimum Windows Vista. Microsoft documents that a session-0 loopback client can capture audio from all user sessions. Session-0 microphone consent behaviour is not verified. The windows-service 0.8.1 SESSION_CHANGE variant and payload type could not be verified from docs.rs. Recommended security stance: install the agent and service under Program Files with quoted service paths, so the binary is not in a user-writable folder.

### Facts

- [high] Latest crates.io versions: wasapi 0.27.0 (updated 2026-11-06), windows 0.62.2 (updated 2025-10-06), ringbuf 0.6.0 (updated 2026-10-30), nnnoiseless 0.5.2 (2025-12-18), windows-service 0.8.1 (2026-05-08). The project's pinned wasapi 0.15 and windows 0.58 are old. (https://crates.io/api/v1/crates/wasapi)
- [high] windows-service 0.8.1 depends on windows-sys ^0.61 with features Win32_Foundation, Win32_Storage_FileSystem, Win32_System_Power, Win32_System_RemoteDesktop, Win32_System_Services, Win32_System_SystemServices, Win32_System_Threading, Win32_System_WindowsProgramming, Win32_UI_WindowsAndMessaging. (https://crates.io/api/v1/crates/windows-service/0.8.1/dependencies)
- [medium] windows 0.62.2 exposes the feature flags Win32_Security, Win32_System_Environment, Win32_System_RemoteDesktop, Win32_System_Services, Win32_System_Threading and Win32_UI_WindowsAndMessaging. (https://docs.rs/crate/windows/0.62.2/features)
- [high] WTSQueryUserToken must be called from a LocalSystem service holding SE_TCB_NAME. Header wtsapi32.h, lib Wtsapi32.lib, DLL Wtsapi32.dll, min Windows Vista. (https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken)
- [high] VirtualDrivers/Virtual-Audio-Driver is MIT licensed (pushed 2026-03-03). It creates a virtual speaker and a virtual microphone via WDK. Its README says it is in beta and requires test signing to be enabled. (https://raw.githubusercontent.com/VirtualDrivers/Virtual-Audio-Driver/master/README.md)
- [high] VB-CABLE is described as donationware with paid commercial licences. The page gives only a generic setup instruction (run Setup as administrator). It does not state redistribution terms, silent-install switches, driver signing status, or the endpoint names. (https://vb-audio.com/Cable/)
- [medium] SteelSeries Sonar creates per-channel virtual playback devices named 'SteelSeries Sonar - Game', '- Chat', and '- Media' (Aux is optional), plus 'SteelSeries Sonar - Microphone' as the input name in Steam/Sonar docs. Names differ across versions; older builds used 'SteelSeries Sonar – Gaming'. Devices are removed when Sonar is disabled. (https://support.steelseries.com/hc/en-us/articles/16955010049933-Why-do-I-have-so-many-Virtual-devices-in-Windows)
- [medium] Loopback capture from a session-0 service can capture the mix of audio from all user sessions. (https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording?redirectedfrom=MSDN)

### Gotchas

- The requested pins are stale: wasapi is now 0.27.0 and windows is 0.62.2. Module paths and feature names changed, so verify against the new docs before coding.
- VirtualDrivers/Virtual-Audio-Driver needs test signing, which conflicts with the no-test-mode goal. A virtual endpoint that works without test mode likely needs an EV or WHQL-signed driver, or you ship an existing signed one.
- VB-CABLE redistribution and auto-install terms are not confirmed on the public page. Read the VB-CABLE reference manual or contact VB-Audio before bundling or silently installing it.
- Per-user audio state (default endpoint, per-app volume, mic privacy consent) belongs to the interactive session. A LocalSystem service launching the agent into the console session via CreateProcessAsUserW must set lpDesktop to winsta0\\default and pass the environment block from CreateEnvironmentBlock.
- The SESSION_CHANGE control and its payload type in windows-service 0.8.1 could not be verified from docs.rs. Check the crate source before relying on the exact names.
- Security: an unquoted service binary path or a binary in a user-writable folder allows privilege escalation. Install under Program Files, quote the ImagePath, and restrict the folder ACL. This is general Windows practice, not verified against a single source here.
- Session-0 microphone consent behaviour (ActivateAudioInterfaceAsync from a service) is unverified. Test the mic capture from the per-user agent process, not from the service.

### Open questions

- Does VB-CABLE allow redistribution and silent install, and what are its exact endpoint friendly names (CABLE Input / CABLE Output)? Confirm from the reference manual or by contacting VB-Audio.
- Is there an open-source, WHQL- or EV-signed virtual audio driver that installs without test signing? Scream and others were not researched.
- What are the exact windows 0.62 module paths for WTSGetActiveConsoleSessionId, WTSQueryUserToken, CreateEnvironmentBlock, CreateProcessAsUserW and the required feature flags? The docs.rs pages did not return usable content in this run.
- What exact Rust names does windows-service 0.8.1 use for SESSION_CHANGE and the session notification payload, and what is the ServiceFailureActions API?
- What is the exact current Sonar Windows endpoint naming in the user's installed version? The names reported by support pages may vary across Sonar releases.
- Does ActivateAudioInterfaceAsync for microphone capture work from a session-0 service, or must the capture run in the per-user agent process?

### Code snippets


## crates

### Summary

Research for a Rust Windows 11 audio tool (Sonar-like). Latest crate versions verified on crates.io, plus source-level checks of tray-icon, muda and tiny_http. Latest versions differ from the project's pins for wasapi (0.15 pinned, 0.27.0 latest), windows (0.58 pinned, 0.62.2 latest), ringbuf (0.4 pinned, 0.6.0 latest). Edge app-mode and AttachConsole claims are partly unverified (see open_questions).

1. tiny_http: latest 0.12.0 (MIT OR Apache-2.0, no rust_version declared). The README still says "0.11", so it is stale. API from src: Server::http(addr) -> Result<Server, Box<dyn Error>>; server.recv_timeout(Duration) -> io::Result<Option<Request>>; incoming_requests(); request.method()/url()/headers()/remote_addr() -> Option<&SocketAddr>; request.as_reader() for the body; request.respond(Response) -> Result. Response::from_string / from_data; .with_header(Header::from_bytes(b"Content-Type", b"application/json").unwrap()); with_status_code; Response::empty(code). Chunked streaming: Response::with_chunked_threshold(0) or with_data(reader, None) gives chunked output. For SSE, request.into_writer() exists but you write the headers by hand. Recommendation: skip SSE and use short polling (GET /api/state every 1s) for a simple dashboard. Bind 127.0.0.1 only, which is the default for a 127.0.0.1:port address.

2. Localhost API hardening: (a) Host header must equal exactly 127.0.0.1:PORT or localhost:PORT, which blocks DNS rebinding. (b) If an Origin header is present it must equal http://127.0.0.1:PORT or http://localhost:PORT, else 403. (c) Require a random per-install token (32 bytes, hex, stored in %APPDATA%\<app>\token, readable by the user only) in a custom header such as X-Api-Token. A custom header forces a CORS preflight, which the server never answers, so cross-site pages cannot send state-changing requests. Serve the dashboard HTML from the same origin, and inject the token into it at serve time. Reject non-GET requests without the token. This is the concise recommended approach. Origin/Host checks alone are not enough without the token.

3. tray-icon: latest 0.26.1 (MIT OR Apache-2.0, rust_version 1.90, which 1.96 satisfies). Depends on muda ^0.21 and windows-sys >=0.60,<=0.61. README: on Windows an event loop must run on the thread that created the icon. winit/tao are not required, since a plain Win32 message loop is enough. Icon::from_rgba(Vec<u8>, w, h) -> Result<Icon, BadIcon> (RGBA8). TrayIconBuilder::new().with_menu(Box<dyn ContextMenu>).with_tooltip(..).with_icon(icon).build() -> Result<TrayIcon>. Events: MenuEvent::receiver() -> &MenuEventReceiver (try_recv -> event.id), TrayIconEvent::receiver(). Also set_event_handler(Some(closure)) exists. The Windows backend creates a hidden window with CreateWindowExW and uses custom WM_USER messages (6002 to 6009), so the thread must pump messages. Pattern: create the tray and menu on the main thread, then run GetMessageW / TranslateMessage / DispatchMessageW in a loop. Poll MenuEvent::receiver().try_recv() after each dispatch, or use set_event_handler to PostThreadMessageW a WM_APP wake-up. muda: latest 0.21.2 (Apache-2.0 OR MIT). Win32 backend is always on. Accelerators need TranslateAcceleratorW, which the tray does not need. Menu::append_items(&[&MenuItem]) and MenuItem::new(text, enabled, None), with id() for matching.

4. Edge app window: msedge.exe usually lives at C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe (from a Microsoft Q&A answer; a third-party guide gave the same path). Preferred lookup: HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe default value. I did NOT verify that App Paths entry for Edge, so it is unverified. Launch with the full path and the explicit .exe, since bare `msedge` failed in a recent build. Args: --app=http://127.0.0.1:PORT --user-data-dir=<own dir> (a separate profile keeps the window from attaching to the user's main Edge session). The --app flag is widely used for chromeless windows, but I did not verify it from a primary source. Fallback: ShellExecuteW(NULL, "open", url) to launch the default browser, avoiding cmd /c start, which mangles & characters.

5. wasapi: latest 0.27.0 (MIT, rust-version 1.85, published 2026-11-06). Releases: 0.26.0 (2026-10-11), 0.25.0 (2026-10-01), 0.24.0 (2026-08-12, MSRV 1.76). 0.15.0 is from 2024-06-17 with MSRV 1.59. The CHANGELOG URL returned 404, so the breaking changes between 0.15 and 0.27 are unverified. Staying on 0.15 is fine for the proof of concept, but any upgrade means reading the source diff.

6. Console and subsystem: #![windows_subsystem = "windows"] at the crate root makes the exe GUI-subsystem, so no console window. Use #![cfg_attr(...)] only if you want the release-only behavior. At the top of main, call AttachConsole(ATTACH_PARENT_PROCESS) (u32::MAX) when CLI args are present. It fails silently when launched from Explorer or the service, which is the intended behavior. Stdio handles may be invalid until attached. todoke 2.2.1 rebinds with SetStdHandle to CONOUT$/CONIN$ after attach, so plan for that. Source: Microsoft AttachConsole docs and the rust-lang/rust#67159 issue. Gotcha: the shell prompt returns right away because the GUI process is not waited on, so CLI output can interleave with the prompt.

Crate versions verified now (crates.io): tiny_http 0.12.0, tray-icon 0.26.1, muda 0.21.2, wasapi 0.27.0, windows 0.62.2, nnnoiseless 0.5.2 (BSD-3-Clause), ringbuf 0.6.0, serde_json 1.0.154.

### Facts

- [high] tiny_http latest version is 0.12.0 (MIT OR Apache-2.0); README still shows tiny_http = "0.11" (https://crates.io/api/v1/crates/tiny_http)
- [high] tray-icon latest is 0.26.1, rust_version 1.90, depends on muda ^0.21 and windows-sys >=0.60,<=0.61 (https://crates.io/api/v1/crates/tray-icon/0.26.1/dependencies)
- [high] muda latest is 0.21.2, license Apache-2.0 OR MIT, rust_version 1.90 (https://crates.io/api/v1/crates/muda)
- [high] wasapi latest is 0.27.0 (rust-version 1.85, published 2026-11-06); 0.15.0 published 2024-06-17 with rust_version 1.59 (https://crates.io/api/v1/crates/wasapi/versions)
- [high] windows crate latest is 0.62.2 (rust_version 1.82); 0.58.0 was published 2024-07-03 with rust_version 1.70 (https://crates.io/api/v1/crates/windows)
- [high] nnnoiseless latest is 0.5.2, BSD-3-Clause; ringbuf latest is 0.6.0 (not 0.4) (https://crates.io/api/v1/crates/nnnoiseless)
- [high] On Windows, tray-icon needs an event loop running on the creating thread; winit/tao is not required by the library (https://raw.githubusercontent.com/tauri-apps/tray-icon/dev/README.md)
- [high] tray-icon Windows backend creates a hidden window with CreateWindowExW and custom WM_USER messages 6002 to 6009 (https://raw.githubusercontent.com/tauri-apps/tray-icon/dev/src/platform_impl/windows/mod.rs)
- [high] tiny_http Server::http returns Result; recv_timeout, incoming_requests, as_reader, into_writer, remote_addr and with_chunked_threshold exist in 0.12 source (https://raw.githubusercontent.com/tiny-http/tiny-http/master/src/lib.rs)
- [high] muda accelerators on Windows need TranslateAcceleratorW in the message loop; MenuEvent::receiver and set_event_handler exist (https://raw.githubusercontent.com/tauri-apps/muda/dev/README.md)
- [medium] A Windows process with windows_subsystem=windows has invalid standard handles until AttachConsole is called (unless the parent passed them); AttachConsole(ATTACH_PARENT_PROCESS) is the common fix (https://github.com/rust-lang/rust/issues/67159)
- [medium] msedge.exe default path is C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe (https://learn.microsoft.com/en-us/answers/questions/2381469/windows-cannot-find-c-program-files-(x86)microsoft?page=6)
- [low] --app=URL opens a chromeless window in Edge (https://textslashplain.com/2022/01/05/edge-command-line-arguments)

### Gotchas

- tiny_http README is stale (says 0.11); the crate is 0.12.0.
- tray-icon on Windows needs the creating thread to pump messages; a sleeping main thread means no menu clicks.
- muda and tray-icon event receivers are global channels; only one set_event_handler per type for the whole process.
- wasapi 0.15 pin is old; the CHANGELOG URL 404s, so API breaks since 0.15 are unverified.
- windows 0.58 pin: windows 0.62.2 is current; the crate features differ across versions.
- ringbuf pinned 0.4 while 0.6.0 is latest; API may differ.
- Windows-subsystem exe: the shell prompt returns immediately, so CLI output can interleave with the next prompt.
- Edge --app= chromeless behavior and msedge App Paths registry entry were NOT confirmed from primary sources.
- ATTACH_PARENT_PROCESS fails silently from Explorer or a service, which is intended, but check GetLastError when debugging.
- Localhost API: Host and Origin checks alone do not stop a malicious page that already knows the port; the random token header is required.

### Open questions

- Does Edge's App Paths registry key for msedge.exe exist on this machine (HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe)? Verify with reg query on the target PC.
- Does --app=URL produce a chromeless window on current Edge (Chromium) builds? Verify by launching once on Windows 11.
- Does Rust std println! after AttachConsole work without SetStdHandle rebinding in the windows crate build? Test on the target machine.
- Breaking changes between wasapi 0.15 and 0.27.0? Read the crate source diff or docs.rs changelog before upgrading.
- Does the windows crate 0.58 export ATTACH_PARENT_PROCESS and AttachConsole under the Win32::System::Console feature? Confirm via docs.rs for 0.58 specifically.
- Is ringbuf 0.6 API compatible with the project's current 0.4 usage? Needs a compile check.

### Code snippets


**tiny_http minimal server returning a response (bind address, incoming loop)** — https://raw.githubusercontent.com/tiny-http/tiny-http/master/examples/readme-example.rs (MIT OR Apache-2.0 (tiny-http crate license; example copied minimally). For the project, bind 127.0.0.1 instead of 0.0.0.0.)

```rust
use tiny_http::{Server, Response};

fn main() {
    let server = Server::http("0.0.0.0:8000").unwrap();
    for request in server.incoming_requests() {
        let response = Response::from_string("hello world");
        request.respond(response).expect("Responded");
    }
}
```

**tray-icon + muda menu event handling (tao shown; for the plain Win32 loop, replace the event loop with GetMessageW/DispatchMessageW and poll MenuEvent::receiver().try_recv())** — https://raw.githubusercontent.com/tauri-apps/tray-icon/dev/examples/tao.rs (Apache-2.0 OR MIT (tray-icon example header: SPDX Apache-2.0 and MIT))

```rust
let tray_menu = Menu::new();
let quit_i = MenuItem::new("Quit", true, None);
tray_menu.append_items(&[&quit_i]);
// on creation thread:
let tray_icon = TrayIconBuilder::new()
    .with_menu(Box::new(tray_menu.clone()))
    .with_tooltip("app")
    .with_icon(icon)
    .build().unwrap();
// event:
if let Ok(event) = MenuEvent::receiver().try_recv() {
    if event.id == quit_i.id() { /* exit */ }
}
```

**Icon from RGBA bytes (signature only)** — https://raw.githubusercontent.com/tauri-apps/tray-icon/dev/src/icon.rs (Apache-2.0 OR MIT)

```rust
tray_icon::Icon::from_rgba(rgba_vec, width, height) // -> Result<Icon, BadIcon>
```

**Attach to parent console when run from a shell, keep the exe GUI-subsystem (original code, not copied from any source)** — https://github.com/rust-lang/rust/issues/67159 (original snippet written for this report; the rust-lang issue is an issue thread, not copied code)

```rust
#![windows_subsystem = "windows"]
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
fn main() {
    unsafe { let _ = AttachConsole(ATTACH_PARENT_PROCESS); }
    // then rebind stdio if needed
}
```
