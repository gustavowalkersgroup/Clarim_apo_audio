//! Windows Audio going away under a running engine: the user restarted it ("Reiniciar audio" in the
//! dashboard, after attaching Equalizer APO to a device) or the service crashed.
//!
//! What the streams see then is a handful of well-known errors (the audio service stopped, the endpoint
//! was invalidated, the RPC link to the service broke). They are not a broken device, so the engine says
//! "reconnecting" instead of a red error, and the agent's retry (`MicEngine::apply` with the same
//! settings, every few seconds with a growing pause) brings the mic back as soon as the service is.

/// Prefix of the status text while the engine waits for Windows Audio. The dashboard keeps texts that start
/// with it out of the red error state (a soft note), like "Calibração" or "Ouvir meu microfone".
pub const RECONNECTING: &str = "Reconectando ao áudio do Windows";

pub(crate) const RECONNECTING_TEXT: &str = "Reconectando ao áudio do Windows (o serviço de áudio parou ou foi reiniciado)";

/// HRESULTs, lower-case hex as `windows::core::Error` prints them ("... (0x88890004)"), that mean Windows
/// Audio went away or handed back an invalid endpoint:
/// `AUDCLNT_E_DEVICE_INVALIDATED`, `AUDCLNT_E_SERVICE_NOT_RUNNING`, `AUDCLNT_E_RESOURCES_INVALIDATED`,
/// `AUDCLNT_E_ENDPOINT_CREATE_FAILED`, `HRESULT_FROM_WIN32(ERROR_SERVICE_NOT_ACTIVE)`,
/// `RPC_S_SERVER_UNAVAILABLE`, `RPC_S_CALL_FAILED`, `RPC_E_DISCONNECTED`.
const SERVICE_LOSS_CODES: [&str; 8] = [
    "0x88890004",
    "0x88890010",
    "0x88890026",
    "0x8889000f",
    "0x80070426",
    "0x800706ba",
    "0x800706be",
    "0x80010108",
];

/// Does this error text (a `Display` of a Windows / WASAPI error) say that Windows Audio is gone or restarting?
pub fn is_audio_service_loss(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    SERVICE_LOSS_CODES.iter().any(|code| detail.contains(code))
}

/// The status text for a stream failure: the "reconnecting" note when `detail` is a Windows Audio loss,
/// otherwise `<user>: <detail>`.
pub(crate) fn failure_text(user: &str, detail: &str) -> String {
    if is_audio_service_loss(detail) {
        RECONNECTING_TEXT.to_string()
    } else {
        format!("{user}: {detail}")
    }
}

/// True for the status text `failure_text` / the engine give while waiting for Windows Audio.
pub fn is_reconnecting(message: &str) -> bool {
    message.starts_with(RECONNECTING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_service_loss_is_recognised_in_windows_error_text_whatever_the_language_or_case() {
        for text in [
            "O dispositivo de ponto de extremidade de áudio foi desconectado. (0x88890004)",
            "The Windows audio service is not running. (0x88890010)",
            "capture read: AUDCLNT_E_DEVICE_INVALIDATED (0X88890004)",
            "O servidor RPC não está disponível. (0x800706BA)",
            "RPC call failed (0x800706BE)",
            "A chamada RPC foi desconectada (0x80010108)",
            "O serviço não foi iniciado. (0x80070426)",
            "Resources invalidated (0x88890026)",
        ] {
            assert!(is_audio_service_loss(text), "{text}");
        }
        for text in [
            "audio device not found (id {0.0.1.00000000}.{abc})",
            "Element not found. (0x80070490)",
            "Access denied (0x80070005)",
            "Device in use (0x8889000a)",
            "",
        ] {
            assert!(!is_audio_service_loss(text), "{text}");
        }
    }

    #[test]
    fn a_failure_text_says_reconnecting_only_for_a_service_loss() {
        let lost = failure_text("Falha ao ler o microfone", "device invalidated (0x88890004)");
        assert!(is_reconnecting(&lost) && !lost.contains("0x8889"), "{lost}");
        let other = failure_text("Falha ao ler o microfone", "access denied (0x80070005)");
        assert_eq!(other, "Falha ao ler o microfone: access denied (0x80070005)");
        assert!(!is_reconnecting(&other));
        assert!(is_reconnecting(RECONNECTING));
    }
}
