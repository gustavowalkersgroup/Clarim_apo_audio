//! Spins the real server on a free port with a recording mock `Backend` and talks to it over a
//! raw TCP socket (no HTTP client dependency).

use anyhow::{anyhow, Result};
use clarim_core::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<String>>,
    mic: Mutex<Option<MicSettings>>,
    channel: Mutex<Option<(ChannelId, ChannelSettings)>>,
    output_eq: Mutex<Option<(String, Option<OutputEq>)>>,
}

impl Mock {
    fn log(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Backend for Mock {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            version: "test".into(),
            config: Config::default(),
            inputs: vec![],
            outputs: vec![],
            sessions: vec![],
            mic: MicStatus::default(),
            virtual_mic_found: false,
            eq_apo: EqApoStatus::default(),
        }
    }
    fn meters(&self) -> Meters {
        Meters { mic: MicStatus::default(), sessions: vec![(42, 0.5)] }
    }
    fn set_mic(&self, s: MicSettings) -> Result<()> {
        self.log("set_mic".into());
        *self.mic.lock().unwrap() = Some(s);
        Ok(())
    }
    fn set_channel(&self, id: ChannelId, s: ChannelSettings) -> Result<()> {
        self.log(format!("set_channel {id:?}"));
        *self.channel.lock().unwrap() = Some((id, s));
        Ok(())
    }
    fn assign_app(&self, exe: &str, ch: Option<ChannelId>) -> Result<()> {
        self.log(format!("assign {exe} {ch:?}"));
        if exe == "boom.exe" {
            return Err(anyhow!("falha de teste"));
        }
        Ok(())
    }
    fn set_routing_enabled(&self, on: bool) -> Result<()> {
        self.log(format!("routing {on}"));
        Ok(())
    }
    fn start_calibration(&self, seconds: f32) -> Result<()> {
        self.log(format!("calibrate {seconds}"));
        Ok(())
    }
    fn set_output_eq(&self, device_id: &str, eq: Option<OutputEq>) -> Result<()> {
        self.log(format!("output_eq {device_id} {}", eq.is_some()));
        *self.output_eq.lock().unwrap() = Some((device_id.to_string(), eq));
        Ok(())
    }
    fn install_eq_apo(&self) -> Result<()> {
        self.log("eq_apo_install".into());
        Ok(())
    }
    fn set_eq_device(&self, device_id: &str, enabled: bool) -> Result<()> {
        self.log(format!("eq_device {device_id} {enabled}"));
        if device_id == "boom" {
            return Err(anyhow!("Este aparelho não aceita o equalizador."));
        }
        Ok(())
    }
    fn restart_audio(&self) -> Result<()> {
        self.log("restart_audio".into());
        Ok(())
    }
    fn finish_onboarding(&self) -> Result<()> {
        self.log("finish_onboarding".into());
        Ok(())
    }
    fn refresh_devices(&self) {
        self.log("refresh".into());
    }
}

struct Resp {
    status: u16,
    headers: String,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        let prefix = format!("{}:", name.to_lowercase());
        self.headers.lines().find(|l| l.to_lowercase().starts_with(&prefix)).map(|l| l[prefix.len()..].trim())
    }
}

struct Server {
    port: u16,
    backend: Arc<Mock>,
    token: String,
}

impl Server {
    fn start() -> Server {
        // Free port: bind to 0, read it, release it (serve() binds it again right away).
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let backend = Arc::new(Mock::default());
        clarim_web::serve(backend.clone(), port).expect("serve");
        let mut s = Server { port, backend, token: String::new() };
        let page = s.get("/", &[]);
        assert_eq!(page.status, 200);
        s.token = extract_token(&page.body);
        s
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Sends one request with `Connection: close` and reads the whole response.
    fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            req += &format!("Host: {}\r\n", self.host());
        }
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += &format!("Connection: close\r\nContent-Length: {}\r\n\r\n", body.len());
        let mut sock = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(req.as_bytes()).unwrap();
        sock.write_all(body).unwrap();
        let mut out = Vec::new();
        sock.read_to_end(&mut out).unwrap();
        let text = String::from_utf8_lossy(&out).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let (status_line, headers) = head.split_once("\r\n").unwrap_or((head, ""));
        let status = status_line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Resp { status, headers: headers.to_string(), body: body.to_string() }
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Resp {
        self.raw("GET", path, headers, b"")
    }

    fn authed_get(&self, path: &str) -> Resp {
        self.get(path, &[("X-Clarim-Token", &self.token)])
    }

    fn post(&self, path: &str, body: &str) -> Resp {
        self.raw("POST", path, &[("X-Clarim-Token", &self.token), ("Content-Type", "application/json")], body.as_bytes())
    }
}

fn extract_token(html: &str) -> String {
    let marker = "window.CLARIM_TOKEN = \"";
    let start = html.find(marker).expect("token assignment in page") + marker.len();
    html[start..].split('"').next().unwrap().to_string()
}

#[test]
fn index_has_injected_token_and_no_placeholder() {
    let s = Server::start();
    let page = s.get("/", &[]);
    assert_eq!(page.status, 200);
    assert!(!page.body.contains(clarim_web::TOKEN_PLACEHOLDER));
    assert_eq!(s.token.len(), 32);
    assert!(s.token.bytes().all(|c| c.is_ascii_hexdigit()));
    assert!(page.header("content-type").unwrap().starts_with("text/html"));
    assert_eq!(page.header("cache-control"), Some("no-store"));
    assert!(page.header("content-security-policy").is_some());
    assert!(page.header("access-control-allow-origin").is_none());
    // the token served to the page is the one the API accepts
    assert_eq!(s.authed_get("/api/state").status, 200);
    // favicon is served and is an SVG
    let icon = s.get("/favicon.svg", &[]);
    assert_eq!(icon.status, 200);
    assert!(icon.body.starts_with("<svg"));
}

#[test]
fn bad_host_is_rejected() {
    let s = Server::start();
    for host in ["evil.example", "evil.example:80", "127.0.0.1", "127.0.0.1:1", "localhost"] {
        assert_eq!(s.get("/", &[("Host", host)]).status, 403, "page with Host {host}");
        assert_eq!(s.get("/api/state", &[("Host", host), ("X-Clarim-Token", &s.token)]).status, 403, "api with Host {host}");
    }
    let localhost = format!("localhost:{}", s.port);
    assert_eq!(s.get("/", &[("Host", &localhost)]).status, 200);
}

#[test]
fn api_requires_valid_token() {
    let s = Server::start();
    assert_eq!(s.get("/api/state", &[]).status, 403);
    assert_eq!(s.get("/api/meters", &[]).status, 403);
    assert_eq!(s.get("/api/state", &[("X-Clarim-Token", "0123456789abcdef0123456789abcdef")]).status, 403);
    assert_eq!(s.get("/api/state", &[("X-Clarim-Token", "")]).status, 403);
    let denied = s.raw("POST", "/api/mic", &[("Content-Type", "application/json")], b"{}");
    assert_eq!(denied.status, 403);
    assert!(denied.body.contains("\"error\""));
    let wrong = s.raw("POST", "/api/mic", &[("X-Clarim-Token", "nope")], b"{}");
    assert_eq!(wrong.status, 403);
    assert!(s.backend.calls().is_empty(), "backend must not be reached: {:?}", s.backend.calls());
}

#[test]
fn wrong_origin_is_rejected() {
    let s = Server::start();
    let t = s.token.clone();
    let bad = s.raw("POST", "/api/mic", &[("X-Clarim-Token", &t), ("Origin", "http://evil.example")], b"{}");
    assert_eq!(bad.status, 403);
    let null = s.get("/api/state", &[("X-Clarim-Token", &t), ("Origin", "null")]);
    assert_eq!(null.status, 403);
    let other_port = s.get("/api/state", &[("X-Clarim-Token", &t), ("Origin", "http://127.0.0.1:1")]);
    assert_eq!(other_port.status, 403);
    assert!(s.backend.calls().is_empty());
    let good_origin = format!("http://127.0.0.1:{}", s.port);
    assert_eq!(s.get("/api/state", &[("X-Clarim-Token", &t), ("Origin", &good_origin)]).status, 200);
}

#[test]
fn options_is_405_without_cors_headers() {
    let s = Server::start();
    for path in ["/", "/api/mic", "/api/state", "/nope"] {
        let r = s.raw("OPTIONS", path, &[("Origin", "http://evil.example"), ("Access-Control-Request-Method", "POST")], b"");
        assert_eq!(r.status, 405, "OPTIONS {path}");
        assert!(!r.headers.to_lowercase().contains("access-control-"), "CORS header leaked: {}", r.headers);
    }
}

#[test]
fn channel_round_trip_reaches_backend() {
    let s = Server::start();
    let r = s.post("/api/channel", r#"{"id":"chat","settings":{"device_id":"{0.0.0.1}.abc","volume":0.35,"muted":true}}"#);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body, r#"{"ok":true}"#);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let (id, settings) = s.backend.channel.lock().unwrap().clone().expect("backend saw the call");
    assert_eq!(id, ChannelId::Chat);
    assert_eq!(settings, ChannelSettings { device_id: Some("{0.0.0.1}.abc".into()), volume: 0.35, muted: true });
}

#[test]
fn volume_is_clamped_and_mic_settings_round_trip() {
    let s = Server::start();
    assert_eq!(s.post("/api/channel", r#"{"id":"game","settings":{"volume":3.5}}"#).status, 200);
    assert_eq!(s.backend.channel.lock().unwrap().clone().unwrap().1.volume, 1.0);

    let mut mic = MicSettings { denoise: DenoiseMode::Light, eq: EqPreset::Warm, voice_focus: true, ..MicSettings::default() };
    mic.input_gain_db = 55.0;
    let r = s.post("/api/mic", &serde_json::to_string(&mic).unwrap());
    assert_eq!(r.status, 200, "{}", r.body);
    let got = s.backend.mic.lock().unwrap().clone().unwrap();
    assert_eq!(got.denoise, DenoiseMode::Light);
    assert_eq!(got.eq, EqPreset::Warm);
    assert!(got.voice_focus);
    assert_eq!(got.input_gain_db, 20.0);
}

#[test]
fn leak_warnings_and_mic_volume_switches_round_trip() {
    let s = Server::start();
    let off = MicSettings { leak_warnings: false, auto_mic_volume: false, ..MicSettings::default() };
    let r = s.post("/api/mic", &serde_json::to_string(&off).unwrap());
    assert_eq!(r.status, 200, "{}", r.body);
    let got = s.backend.mic.lock().unwrap().clone().unwrap();
    assert!(!got.leak_warnings && !got.auto_mic_volume, "both switches must reach the backend off: {got:?}");
    // the other one alone, and back on
    let one = MicSettings { auto_mic_volume: false, ..MicSettings::default() };
    assert_eq!(s.post("/api/mic", &serde_json::to_string(&one).unwrap()).status, 200);
    let got = s.backend.mic.lock().unwrap().clone().unwrap();
    assert!(got.leak_warnings && !got.auto_mic_volume, "leak_warnings on, auto_mic_volume off: {got:?}");
}

#[test]
fn a_mic_post_without_the_call_switches_keeps_them_on() {
    let s = Server::start();
    let r = s.post("/api/mic", r#"{"denoise":"light"}"#);
    assert_eq!(r.status, 200, "{}", r.body);
    let got = s.backend.mic.lock().unwrap().clone().unwrap();
    assert_eq!(got.denoise, DenoiseMode::Light);
    assert!(got.leak_warnings && got.auto_mic_volume, "old clients keep the defaults: {got:?}");
    assert_eq!(got.mic_volume_taken, None);
}

#[test]
fn mic_status_json_has_the_keys_the_dashboard_reads() {
    let s = Server::start();
    let body: serde_json::Value = serde_json::from_str(&s.authed_get("/api/meters").body).unwrap();
    for key in [
        "agc_gain_db", "voice_db", "echo_score", "echo_lag_ms", "noise_db", "references", "in_call", "call_apps", "leak",
        "mic_volume_note", "mic_too_quiet",
    ] {
        assert!(body["mic"].get(key).is_some(), "meters.mic lost `{key}`: {body}");
    }
    let state: serde_json::Value = serde_json::from_str(&s.authed_get("/api/state").body).unwrap();
    for key in ["leak_warnings", "auto_mic_volume"] {
        assert!(state["config"]["mic"].get(key).is_some(), "config.mic lost `{key}`: {state}");
    }
}

#[test]
fn mic_tone_sliders_round_trip_and_are_clamped() {
    let s = Server::start();
    let r = s.post("/api/mic", r#"{"eq":"clarity","eq_bass_db":3.5,"eq_mid_db":-40,"eq_treble_db":99}"#);
    assert_eq!(r.status, 200, "{}", r.body);
    let got = s.backend.mic.lock().unwrap().clone().unwrap();
    assert_eq!((got.eq, got.eq_bass_db, got.eq_mid_db, got.eq_treble_db), (EqPreset::Clarity, 3.5, -12.0, 12.0));
}

#[test]
fn output_eq_round_trip_clamps_and_removes() {
    let s = Server::start();
    let dev = "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}";
    let r = s.post("/api/output_eq", &format!(r#"{{"device_id":"{dev}","eq":{{"bass_db":5,"mid_db":-99,"treble_db":1.5}}}}"#));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body, r#"{"ok":true}"#);
    let (id, eq) = s.backend.output_eq.lock().unwrap().clone().expect("backend saw the call");
    assert_eq!(id, dev);
    assert_eq!(eq, Some(OutputEq { bass_db: 5.0, mid_db: -12.0, treble_db: 1.5 }));
    // a missing gain is 0
    assert_eq!(s.post("/api/output_eq", &format!(r#"{{"device_id":"{dev}","eq":{{"treble_db":4}}}}"#)).status, 200);
    assert_eq!(
        s.backend.output_eq.lock().unwrap().clone().unwrap().1,
        Some(OutputEq { bass_db: 0.0, mid_db: 0.0, treble_db: 4.0 })
    );
    // null removes the entry
    assert_eq!(s.post("/api/output_eq", &format!(r#"{{"device_id":"{dev}","eq":null}}"#)).status, 200);
    assert_eq!(s.backend.output_eq.lock().unwrap().clone().unwrap(), (dev.to_string(), None));
    assert_eq!(s.backend.calls().len(), 3);
}

#[test]
fn output_eq_bad_input_never_reaches_the_backend() {
    let s = Server::start();
    for body in [
        r#"{"device_id":"","eq":null}"#,
        r#"{"device_id":"   ","eq":{"bass_db":1}}"#,
        &format!(r#"{{"device_id":"{}","eq":null}}"#, "x".repeat(513)),
        r#"{"device_id":"{0.0.0.0}.{x}"}"#,        // `eq` missing: not a removal
        r#"{"eq":{"bass_db":1}}"#,                 // no device
        r#"{"device_id":"d","eq":{"bass_db":1e99}}"#, // overflows f32: refused, not clamped
        r#"{"device_id":"d","eq":{"bass_db":"loud"}}"#,
        r#"{"device_id":"d","eq":"flat"}"#,
        r#"{"device_id":7,"eq":null}"#,
    ] {
        assert_eq!(s.post("/api/output_eq", body).status, 400, "{body}");
    }
    assert!(s.backend.calls().is_empty(), "{:?}", s.backend.calls());
    // exactly 512 chars is accepted
    let max = format!(r#"{{"device_id":"{}","eq":null}}"#, "x".repeat(512));
    assert_eq!(s.post("/api/output_eq", &max).status, 200);
    // auth and method rules are the same as for the other POST routes
    let denied = s.raw("POST", "/api/output_eq", &[("Content-Type", "application/json")], b"{\"device_id\":\"d\",\"eq\":null}");
    assert_eq!(denied.status, 403);
    let r = s.authed_get("/api/output_eq");
    assert_eq!((r.status, r.header("allow")), (405, Some("POST")));
    assert_eq!(s.backend.calls().len(), 1);
}

#[test]
fn equalizer_apo_install_device_and_audio_restart_reach_the_backend() {
    let s = Server::start();
    let dev = "{0.0.0.00000000}.{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}";
    // install and restart take no body
    let r = s.post("/api/eq_apo/install", "");
    assert_eq!((r.status, r.body.as_str()), (200, r#"{"ok":true}"#), "{}", r.body);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(s.post("/api/audio/restart", "").status, 200);
    // one device, on and off
    assert_eq!(s.post("/api/eq_apo/device", &format!(r#"{{"device_id":"{dev}","enabled":true}}"#)).status, 200);
    assert_eq!(s.post("/api/eq_apo/device", &format!(r#"{{"device_id":"{dev}","enabled":false}}"#)).status, 200);
    assert_eq!(
        s.backend.calls(),
        ["eq_apo_install".to_string(), "restart_audio".to_string(), format!("eq_device {dev} true"), format!("eq_device {dev} false")]
    );
    // the backend's refusal is a 400 with its PT-BR text
    let r = s.post("/api/eq_apo/device", r#"{"device_id":"boom","enabled":true}"#);
    assert_eq!(r.status, 400);
    assert!(r.body.contains("não aceita o equalizador"), "{}", r.body);
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
}

#[test]
fn the_first_steps_card_is_dismissed_with_a_bodyless_post() {
    let s = Server::start();
    let r = s.post("/api/onboarding/done", "");
    assert_eq!((r.status, r.body.as_str()), (200, r#"{"ok":true}"#), "{}", r.body);
    assert_eq!(s.backend.calls(), ["finish_onboarding".to_string()]);
    let state: serde_json::Value = serde_json::from_str(&s.authed_get("/api/state").body).unwrap();
    assert!(state["config"].get("onboarding_done").is_some(), "config lost `onboarding_done`: {state}");
}

#[test]
fn equalizer_apo_routes_check_input_auth_origin_and_method_like_the_others() {
    let s = Server::start();
    for body in [
        r#"{"device_id":"","enabled":true}"#,
        r#"{"device_id":"   ","enabled":false}"#,
        &format!(r#"{{"device_id":"{}","enabled":true}}"#, "x".repeat(513)),
        r#"{"device_id":"{0.0.0.0}.{x}"}"#,            // `enabled` missing
        r#"{"enabled":true}"#,                         // no device
        r#"{"device_id":"d","enabled":"yes"}"#,
        r#"{"device_id":7,"enabled":true}"#,
        "{ not json",
    ] {
        assert_eq!(s.post("/api/eq_apo/device", body).status, 400, "{body}");
    }
    assert!(s.backend.calls().is_empty(), "{:?}", s.backend.calls());
    // exactly 512 chars is accepted
    let max = format!(r#"{{"device_id":"{}","enabled":false}}"#, "x".repeat(512));
    assert_eq!(s.post("/api/eq_apo/device", &max).status, 200);
    // no token, wrong token, wrong origin: the backend is never reached
    for path in ["/api/eq_apo/install", "/api/eq_apo/device", "/api/audio/restart", "/api/onboarding/done"] {
        let body = br#"{"device_id":"d","enabled":true}"#;
        assert_eq!(s.raw("POST", path, &[("Content-Type", "application/json")], body).status, 403, "{path} without a token");
        assert_eq!(s.raw("POST", path, &[("X-Clarim-Token", "nope")], body).status, 403, "{path} with a wrong token");
        let evil = [("X-Clarim-Token", s.token.as_str()), ("Origin", "http://evil.example")];
        assert_eq!(s.raw("POST", path, &evil, body).status, 403, "{path} from another origin");
        let r = s.authed_get(path);
        assert_eq!((r.status, r.header("allow")), (405, Some("POST")), "{path} by GET");
        let r = s.raw("OPTIONS", path, &[("Origin", "http://evil.example"), ("Access-Control-Request-Method", "POST")], b"");
        assert_eq!(r.status, 405, "{path} preflight");
        assert!(!r.headers.to_lowercase().contains("access-control-"));
    }
    assert_eq!(s.backend.calls().len(), 1, "only the 512-char request got through: {:?}", s.backend.calls());
}

#[test]
fn assign_routing_calibrate_refresh_reach_backend() {
    let s = Server::start();
    assert_eq!(s.post("/api/assign", r#"{"exe":" Spotify.EXE ","channel":"media"}"#).status, 200);
    assert_eq!(s.post("/api/assign", r#"{"exe":"discord.exe","channel":null}"#).status, 200);
    assert_eq!(s.post("/api/routing", r#"{"enabled":false}"#).status, 200);
    assert_eq!(s.post("/api/calibrate", r#"{"seconds":5}"#).status, 200);
    assert_eq!(s.post("/api/refresh", "").status, 200);
    assert_eq!(
        s.backend.calls(),
        ["assign spotify.exe Some(Media)", "assign discord.exe None", "routing false", "calibrate 5", "refresh"]
    );
}

#[test]
fn backend_errors_and_bad_input_are_json_errors() {
    let s = Server::start();
    let r = s.post("/api/assign", r#"{"exe":"boom.exe","channel":"game"}"#);
    assert_eq!(r.status, 400);
    assert!(r.body.contains("falha de teste"), "{}", r.body);
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
    assert_eq!(s.post("/api/assign", r#"{"exe":"..\\evil.exe","channel":null}"#).status, 400);
    assert_eq!(s.post("/api/assign", r#"{"exe":"","channel":null}"#).status, 400);
    assert_eq!(s.post("/api/mic", "{ not json").status, 400);
    assert_eq!(s.post("/api/channel", r#"{"id":"nope","settings":{}}"#).status, 400);
    assert_eq!(s.post("/api/mic", r#"{"denoise":"turbo"}"#).status, 400);
}

#[test]
fn unknown_route_and_wrong_method() {
    let s = Server::start();
    assert_eq!(s.authed_get("/api/nope").status, 404);
    assert_eq!(s.get("/nope", &[]).status, 404);
    let r = s.authed_get("/api/mic");
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("POST"));
    let r = s.post("/api/state", "{}");
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET"));
}

#[test]
fn oversized_body_is_413_and_not_applied() {
    let s = Server::start();
    // declared length over the cap
    let big = format!(r#"{{"id":"chat","settings":{{"device_id":"{}"}}}}"#, "x".repeat(70 * 1024));
    let r = s.post("/api/channel", &big);
    assert_eq!(r.status, 413, "{}", r.body);
    assert!(r.body.contains("error"));
    // exactly at the cap is still read (and then rejected only because it is not valid JSON)
    let at_cap = " ".repeat(64 * 1024);
    assert_eq!(s.post("/api/mic", &at_cap).status, 400);
    assert!(s.backend.calls().is_empty(), "{:?}", s.backend.calls());
    // the server is still healthy afterwards
    assert_eq!(s.authed_get("/api/state").status, 200);
}

#[test]
fn state_and_meters_are_json() {
    let s = Server::start();
    let state: Snapshot = serde_json::from_str(&s.authed_get("/api/state").body).unwrap();
    assert_eq!(state.version, "test");
    assert_eq!(state.config.channels.len(), 4);
    let meters: Meters = serde_json::from_str(&s.authed_get("/api/meters").body).unwrap();
    assert_eq!(meters.sessions, vec![(42, 0.5)]);
}

#[test]
fn port_in_use_is_an_error_not_a_panic() {
    let hold = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = hold.local_addr().unwrap().port();
    assert!(clarim_web::serve(Arc::new(Mock::default()), port).is_err());
}
