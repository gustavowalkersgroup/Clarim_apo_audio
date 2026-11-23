//! Read-only probe (writes nothing): lists the audio sessions on every active render endpoint with
//! the per-app endpoint Windows has persisted for each, and for this process.
//! `cargo run -p clarim-routing --example probe`

use clarim_core::Config;
use clarim_routing::{list_sessions_once, read_persisted_endpoint, RoutingConfig};

fn main() -> anyhow::Result<()> {
    // Config::default(), not load(): load() may rename a corrupt config file.
    let cfg = Config::default();
    let routing = RoutingConfig {
        channels: cfg.channels,
        app_rules: cfg.app_rules,
        default_channel: cfg.default_channel,
        enabled: false,
        call_to_chat: cfg.call_to_chat,
    };
    let own = std::process::id();
    // This process has no audio session, so Windows is expected to refuse (E_INVALIDARG).
    println!("own pid {own} (no audio session): {:?}", read_persisted_endpoint(own).map_err(|e| e.to_string()));
    let sessions = list_sessions_once(&routing)?;
    println!("{} app sessions", sessions.len());
    for s in sessions {
        let persisted = match read_persisted_endpoint(s.pid) {
            Ok(p) => format!("{p:?}"),
            Err(e) => format!("error: {e}"),
        };
        println!(
            "pid {:>6} {:<24} name={:?} channel={:?} rule={} active={} peak={:.3}\n    on {}\n    persisted {persisted}",
            s.pid, s.exe, s.name, s.channel, s.has_rule, s.active, s.peak, s.device_id
        );
    }
    Ok(())
}
