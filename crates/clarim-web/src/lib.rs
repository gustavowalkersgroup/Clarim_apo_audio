//! Local web dashboard + JSON API (127.0.0.1 only).
//!
//! Security model (see SPEC "clarim-web"): Host allow-list (DNS rebinding), per-run random token
//! injected into the served page and required as `X-Clarim-Token` on every `/api/*` request,
//! Origin allow-list, no CORS headers, `OPTIONS` -> 405, 64 KB body cap, `Cache-Control: no-store`.

use anyhow::{anyhow, Context, Result};
use clarim_core::{Backend, ChannelId, ChannelSettings, MicSettings, OutputEq, EQ_TONE_RANGE_DB};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server};

/// Placeholder is replaced with the per-run API token when the page is served.
pub const TOKEN_PLACEHOLDER: &str = "__CLARIM_TOKEN__";

/// The dashboard page, embedded at compile time.
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

const FAVICON_SVG: &str = concat!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">"##,
    r##"<rect width="32" height="32" rx="8" fill="#0e1014"/>"##,
    r##"<path d="M8 14v4M13 10v12M18 6v20M23 12v8" stroke="#4cc2ff" stroke-width="3" stroke-linecap="round" fill="none"/>"##,
    r##"</svg>"##
);

/// Request bodies larger than this are refused with 413.
const MAX_BODY: usize = 64 * 1024;
/// Requests are tiny; a few workers keep one slow backend call from blocking the dashboard.
const WORKERS: usize = 3;
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                   img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

struct Ctx {
    backend: Arc<dyn Backend>,
    port: u16,
    token: String,
    /// Dashboard with the token already injected (constant for the whole run).
    page: String,
}

/// A fully built response, decoupled from `tiny_http` so routing stays easy to read.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    html: bool,
    allow: Option<&'static str>,
}

impl Reply {
    fn json(status: u16, v: serde_json::Value) -> Reply {
        Reply { status, content_type: "application/json; charset=utf-8", body: v.to_string().into_bytes(), html: false, allow: None }
    }
    fn ok() -> Reply {
        Reply::json(200, json!({ "ok": true }))
    }
    fn err(status: u16, msg: impl AsRef<str>) -> Reply {
        Reply::json(status, json!({ "error": msg.as_ref() }))
    }
    fn not_allowed(allow: &'static str) -> Reply {
        Reply { allow: Some(allow), ..Reply::err(405, "Método não permitido.") }
    }
}

/// Binds 127.0.0.1:`port` only and serves on a background thread.
///
/// The returned handle finishes only if the listener dies, so `handle.is_finished()` tells the
/// caller the dashboard is gone.
pub fn serve(backend: Arc<dyn Backend>, port: u16) -> Result<JoinHandle<()>> {
    let server = Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let token = new_token();
    let ctx = Ctx { backend, port, page: DASHBOARD_HTML.replace(TOKEN_PLACEHOLDER, &token), token };
    log::info!("dashboard listening on http://127.0.0.1:{port}/");
    thread::Builder::new()
        .name("clarim-web".into())
        .spawn(move || run(&server, &ctx))
        .context("spawn web thread")
}

fn run(server: &Server, ctx: &Ctx) {
    let dead = AtomicBool::new(false);
    thread::scope(|s| {
        for i in 1..WORKERS {
            let spawned = thread::Builder::new()
                .name(format!("clarim-web-{i}"))
                .spawn_scoped(s, || worker(server, ctx, &dead));
            if let Err(e) = spawned {
                log::warn!("web: cannot spawn worker {i}: {e}");
            }
        }
        worker(server, ctx, &dead);
    });
}

fn worker(server: &Server, ctx: &Ctx, dead: &AtomicBool) {
    while !dead.load(Ordering::Relaxed) {
        match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(req)) => {
                // A panicking backend must not take the dashboard down: the dropped request
                // is answered with 500 by tiny_http.
                if catch_unwind(AssertUnwindSafe(|| handle(ctx, req))).is_err() {
                    log::error!("web: request handler panicked");
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("web: listener stopped: {e}");
                dead.store(true, Ordering::Relaxed);
            }
        }
    }
}

fn handle(ctx: &Ctx, mut req: Request) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("");
    let reply = route(ctx, &mut req, path);
    respond(req, reply);
}

fn route(ctx: &Ctx, req: &mut Request, path: &str) -> Reply {
    // DNS rebinding defence: a page on evil.example resolving to 127.0.0.1 still sends its own Host.
    let host_ok = header(req, "Host")
        .is_some_and(|h| [format!("127.0.0.1:{}", ctx.port), format!("localhost:{}", ctx.port)].iter().any(|a| a.eq_ignore_ascii_case(h)));
    if !host_ok {
        return Reply::err(403, "Host não permitido.");
    }
    let method = req.method().clone();
    if method == Method::Options {
        return Reply::not_allowed("GET, POST"); // no CORS, ever: preflights are never answered
    }
    match (&method, path) {
        (&Method::Get, "/") => Reply {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: ctx.page.clone().into_bytes(),
            html: true,
            allow: None,
        },
        (&Method::Get, "/favicon.svg") => Reply {
            status: 200,
            content_type: "image/svg+xml",
            body: FAVICON_SVG.as_bytes().to_vec(),
            html: false,
            allow: None,
        },
        (_, "/" | "/favicon.svg") => Reply::not_allowed("GET"),
        _ if path.starts_with("/api/") => api(ctx, req, &method, path),
        _ => Reply::err(404, "Não encontrado."),
    }
}

fn api(ctx: &Ctx, req: &mut Request, method: &Method, path: &str) -> Reply {
    if let Some(origin) = header(req, "Origin") {
        let ok = [format!("http://127.0.0.1:{}", ctx.port), format!("http://localhost:{}", ctx.port)]
            .iter()
            .any(|a| a.eq_ignore_ascii_case(origin));
        if !ok {
            return Reply::err(403, "Origem não permitida.");
        }
    }
    let token_ok = header(req, "X-Clarim-Token").is_some_and(|t| ct_eq(t.as_bytes(), ctx.token.as_bytes()));
    if !token_ok {
        return Reply::err(403, "Token ausente ou inválido. Recarregue a página.");
    }

    let b = &*ctx.backend;
    match (method, path) {
        (Method::Get, "/api/state") => to_json(&b.snapshot()),
        (Method::Get, "/api/meters") => to_json(&b.meters()),
        (Method::Post, "/api/mic") => with_body(req, |mut s: MicSettings| {
            sanitize_mic(&mut s);
            b.set_mic(s)
        }),
        (Method::Post, "/api/channel") => with_body(req, |r: ChannelReq| {
            let mut s = r.settings;
            s.volume = s.volume.clamp(0.0, 1.0);
            b.set_channel(r.id, s)
        }),
        (Method::Post, "/api/output_eq") => with_body(req, |r: OutputEqReq| {
            if r.device_id.trim().is_empty() || r.device_id.len() > 512 {
                return Err(anyhow!("Dispositivo inválido."));
            }
            b.set_output_eq(&r.device_id, r.eq.map(OutputEq::clamped).transpose()?)
        }),
        (Method::Post, "/api/assign") => with_body(req, |r: AssignReq| {
            let exe = r.exe.trim().to_lowercase();
            if exe.is_empty() || exe.len() > 260 || exe.contains(['/', '\\']) {
                return Err(anyhow!("Nome de programa inválido."));
            }
            b.assign_app(&exe, r.channel)
        }),
        (Method::Post, "/api/routing") => with_body(req, |r: RoutingReq| b.set_routing_enabled(r.enabled)),
        (Method::Post, "/api/calibrate") => {
            with_body(req, |r: CalibrateReq| b.start_calibration(r.seconds.clamp(1.0, 30.0)))
        }
        (Method::Post, "/api/refresh") => {
            b.refresh_devices();
            Reply::ok()
        }
        // Equalizer APO, done by the Clarim service for the user: install it, attach it to one device, restart
        // Windows Audio. The first and the last take no body.
        (Method::Post, "/api/eq_apo/install") => done(b.install_eq_apo()),
        (Method::Post, "/api/eq_apo/device") => with_body(req, |r: EqDeviceReq| {
            if r.device_id.trim().is_empty() || r.device_id.len() > 512 {
                return Err(anyhow!("Dispositivo inválido."));
            }
            b.set_eq_device(&r.device_id, r.enabled)
        }),
        (Method::Post, "/api/audio/restart") => done(b.restart_audio()),
        (Method::Post, "/api/onboarding/done") => done(b.finish_onboarding()),
        (_, "/api/state" | "/api/meters") => Reply::not_allowed("GET"),
        (
            _,
            "/api/mic" | "/api/channel" | "/api/output_eq" | "/api/assign" | "/api/routing" | "/api/calibrate" | "/api/refresh"
            | "/api/eq_apo/install" | "/api/eq_apo/device" | "/api/audio/restart" | "/api/onboarding/done",
        ) => Reply::not_allowed("POST"),
        _ => Reply::err(404, "Não encontrado."),
    }
}

#[derive(Deserialize)]
struct ChannelReq {
    id: ChannelId,
    settings: ChannelSettings,
}

#[derive(Deserialize)]
struct OutputEqReq {
    device_id: String,
    /// `null` removes the device's EQ; the key itself is required (a missing key is not a removal).
    #[serde(deserialize_with = "Option::deserialize")]
    eq: Option<OutputEq>,
}

#[derive(Deserialize)]
struct EqDeviceReq {
    device_id: String,
    enabled: bool,
}

#[derive(Deserialize)]
struct AssignReq {
    exe: String,
    channel: Option<ChannelId>,
}

#[derive(Deserialize)]
struct RoutingReq {
    enabled: bool,
}

#[derive(Deserialize)]
struct CalibrateReq {
    seconds: f32,
}

/// Reads the (capped) JSON body, runs `f`, maps its result to `{ok:true}` / 400 `{error}`.
fn with_body<T: DeserializeOwned>(req: &mut Request, f: impl FnOnce(T) -> Result<()>) -> Reply {
    let value: T = match read_json(req) {
        Ok(v) => v,
        Err(reply) => return reply,
    };
    done(f(value))
}

/// `{ok:true}`, or 400 `{error}` with the backend's (PT-BR) text.
fn done(result: Result<()>) -> Reply {
    match result {
        Ok(()) => Reply::ok(),
        Err(e) => {
            log::warn!("web: request failed: {e:#}");
            Reply::err(400, format!("{e:#}"))
        }
    }
}

fn read_json<T: DeserializeOwned>(req: &mut Request) -> std::result::Result<T, Reply> {
    let too_big = || Reply::err(413, "Corpo da requisição grande demais (máx. 64 KB).");
    if req.body_length().is_some_and(|n| n > MAX_BODY) {
        return Err(too_big());
    }
    let mut buf = Vec::new();
    // Chunked bodies have no declared length, so the cap is enforced while reading too.
    if req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut buf).is_err() {
        return Err(Reply::err(400, "Não foi possível ler o corpo da requisição."));
    }
    if buf.len() > MAX_BODY {
        return Err(too_big());
    }
    serde_json::from_slice(&buf).map_err(|e| Reply::err(400, format!("JSON inválido: {e}")))
}

fn to_json<T: serde::Serialize>(v: &T) -> Reply {
    match serde_json::to_value(v) {
        Ok(v) => Reply::json(200, v),
        Err(e) => Reply::err(500, format!("Falha ao serializar: {e}")),
    }
}

/// Keeps out-of-range values (hand-made API calls) from reaching the DSP.
fn sanitize_mic(m: &mut MicSettings) {
    m.denoise_strength = m.denoise_strength.clamp(0.0, 1.0);
    m.voice_focus_db = m.voice_focus_db.clamp(-40.0, 0.0);
    m.gate_threshold_db = m.gate_threshold_db.clamp(-90.0, 0.0);
    m.input_gain_db = m.input_gain_db.clamp(-20.0, 20.0);
    m.output_gain_db = m.output_gain_db.clamp(-20.0, 20.0);
    for tone in [&mut m.eq_bass_db, &mut m.eq_mid_db, &mut m.eq_treble_db] {
        *tone = tone.clamp(-EQ_TONE_RANGE_DB, EQ_TONE_RANGE_DB);
    }
    m.voice_level_db = m.voice_level_db.map(|v| v.clamp(-90.0, 0.0));
}

fn respond(req: Request, r: Reply) {
    // Always Content-Length (never chunked): simplest for every client.
    let mut resp = Response::from_data(r.body).with_status_code(r.status).with_chunked_threshold(usize::MAX);
    let mut add = |name: &str, value: &str| {
        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            resp.add_header(h);
        }
    };
    add("Content-Type", r.content_type);
    add("Cache-Control", "no-store");
    add("X-Content-Type-Options", "nosniff");
    if let Some(allow) = r.allow {
        add("Allow", allow);
    }
    if r.html {
        add("Content-Security-Policy", CSP);
        add("X-Frame-Options", "DENY");
        add("Referrer-Policy", "no-referrer");
    }
    if let Err(e) = req.respond(resp) {
        log::debug!("web: client went away: {e}");
    }
}

fn header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

/// Constant-time equality (length is not secret: tokens have a fixed size).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 128 random bits as hex. Each `RandomState` is seeded by the OS, so no `rand` dependency
/// is needed; clock and pid are mixed in as extra input.
fn new_token() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    let a = RandomState::new().hash_one((0u8, nanos, pid));
    let b = RandomState::new().hash_one((1u8, nanos, pid));
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_carries_token_placeholder_once() {
        assert_eq!(DASHBOARD_HTML.matches(TOKEN_PLACEHOLDER).count(), 1);
    }

    #[test]
    fn dashboard_makes_no_external_requests() {
        // The only absolute URLs allowed are the user-clicked download links: VB-Cable (in the "Primeiros passos" card and
        // in the mic card's missing-cable warning) and Equalizer APO.
        let urls: Vec<&str> = DASHBOARD_HTML
            .split(['"', '\'', ' ', '(', ')'])
            .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
            .collect();
        assert_eq!(
            urls,
            ["https://vb-audio.com/Cable/", "https://vb-audio.com/Cable/", "https://sourceforge.net/projects/equalizerapo/"]
        );
        assert!(!DASHBOARD_HTML.contains("@import"));
    }

    /// The page logic is plain JS (checked by hand against the mock example); this only keeps the
    /// user-facing PT-BR notices and the closed "Avançado" section from being dropped by accident.
    #[test]
    fn dashboard_carries_the_required_notices() {
        for needle in [
            "SteelSeries Sonar detectado: se o GG/Sonar estiver aberto, os dois brigam pela saída de cada app.",
            "O SteelSeries Sonar está instalado: feche o GG/Sonar antes de ligar o Roteamento por app.",
            // soft notes of the engine (mic.error) that must stay out of the red error state
            "/^Calibração/",
            "/^Microfone silenciado/",
            "/^Ouvir meu microfone/",
            "/^Reconectando ao áudio do Windows/",
            "'em chamada'",
            "'não roteado'",
            "Sem sinal do microfone. Confira se ele não está mudo",
            "sem calibrar",
            "'Sem regra ('",
            "Indisponível — reconecte ou escolha outro",
            "<details class=\"adv\">",
            // output equalizer (Equalizer APO): Clarim installs and configures it, the user only presses buttons
            "Ativo neste aparelho",
            "As melhorias de áudio do Windows estão desligadas neste aparelho; o equalizador não funciona assim.",
            "Para equalizar o que você ouve, o Clarim instala o Equalizer APO (grátis) por você.",
            "leva ~1 min e o som pode piscar",
            "Instalar equalizador",
            "Equalizador neste aparelho",
            "Falta reiniciar o áudio pra valer",
            "a mudança no equalizador só vale depois disso",
            "ainda toca até reiniciar o áudio",
            "Reiniciar áudio agora",
            "/api/eq_apo/install",
            "/api/eq_apo/device",
            "/api/audio/restart",
            // first steps card (after the guided install)
            "Primeiros passos",
            "CABLE Output",
            "Instalar VB-Cable",
            "Entendi",
            "/api/onboarding/done",
            // calls: voice AGC, leak warnings, Windows mic volume guard
            "Volume automático",
            "Mantém sua voz no mesmo volume na chamada: não estoura nem fica baixa.",
            "Avisar se vazar som na chamada",
            "Ajustar o volume do microfone no Windows se estourar",
            "chk('m-leak', 'leak_warnings')",
            "chk('m-micvol', 'auto_mic_volume')",
            "id=\"agc\"",
            "id=\"call-pill\"",
            "'Em chamada'",
            "id=\"leak\" class=\"banner warn\" role=\"status\"",
            "Tem som das caixas vazando no seu microfone: o pessoal da chamada pode ouvir o jogo, vídeos ou outros sons daí. Abaixe o volume das caixas ou use fone.",
            "Tem ruído de fundo passando pelo filtro na chamada. Aproxime o microfone ou ligue o Foco na voz.",
            " Tente também o filtro Forte.",
            "id=\"micvol\" class=\"banner info\"",
            "id=\"quiet\" class=\"hint\"",
            "Sua voz está chegando baixa mesmo com o volume automático no máximo. Suba o volume do microfone no Windows ou aproxime o microfone.",
            "'Vazamento: eco '",
        ] {
            assert!(DASHBOARD_HTML.contains(needle), "dashboard.html lost: {needle}");
        }
        assert!(!DASHBOARD_HTML.contains("<details class=\"adv\" open"), "Avançado starts closed");
        // the user never opens Equalizer APO's own tools
        for gone in ["Configurator", "DeviceSelector", "Editor.exe", "Restart Audio Service"] {
            assert!(!DASHBOARD_HTML.contains(gone), "dashboard.html still mentions {gone}");
        }
        // the page scrolls instead of squeezing the app lists to a row or two inside fixed-height cards
        assert!(!DASHBOARD_HTML.contains("height: 100vh; min-height: 760px"));
        assert!(DASHBOARD_HTML.contains("min-height: 190px; max-height: 380px"), "app list room: >= 4 rows");
    }

    #[test]
    fn token_is_128_bit_hex_and_unique() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc123", b"abc123"));
        assert!(!ct_eq(b"abc123", b"abc124"));
        assert!(!ct_eq(b"abc123", b"abc12"));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn mic_settings_are_clamped() {
        let mut m = MicSettings {
            denoise_strength: 7.0,
            input_gain_db: -99.0,
            voice_focus_db: 5.0,
            eq_bass_db: 40.0,
            eq_mid_db: -40.0,
            eq_treble_db: 3.5,
            ..MicSettings::default()
        };
        sanitize_mic(&mut m);
        assert_eq!((m.denoise_strength, m.input_gain_db, m.voice_focus_db), (1.0, -20.0, 0.0));
        assert_eq!((m.eq_bass_db, m.eq_mid_db, m.eq_treble_db), (12.0, -12.0, 3.5));
    }
}
