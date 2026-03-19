//! Zodia — p2p astrological encounter network.
//!
//! Entry point: initialise tracing, load or create local config, then hand
//! control to the Relm4 application loop.

mod app;
mod peer_list;
mod util;

use app::{AppInit, AppModel};
use relm4::RelmApp;
use zodia_config::LocalConfig;
use zodia_store::{ZodiaStore, seed::BaselineData};

const BASELINE_TOML: &str = include_str!("../assets/baseline_aspects.toml");

fn main() {
    // ── tracing ───────────────────────────────────────────────────────────────
    // Suppress noisy iroh/p2panda internals (NAT traversal warnings, DHT
    // discovery timeouts) unless the caller explicitly sets ZODIA_LOG.
    // The overrideable default keeps our own crates at info and everything
    // else quiet enough for daily use.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZODIA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(
                    "iroh_quinn_proto=error,\
                     iroh=warn,\
                     p2panda_net=error,\
                     info"
                )),
        )
        .init();

    // ── local config (identity + optional birth data) ─────────────────────────
    let config = match LocalConfig::load_or_create() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fatal: could not load config: {e}");
            std::process::exit(1);
        }
    };

    // ── store (open or create, then seed baseline if empty) ──────────────────
    let store_path = config.data_dir().join("interpretations.db");
    let store = match ZodiaStore::open(&store_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: could not open store: {e}");
            std::process::exit(1);
        }
    };
    match BaselineData::from_toml(BASELINE_TOML) {
        Ok(baseline) => {
            if let Ok(n) = store.seed_if_empty(&baseline) {
                if n > 0 {
                    tracing::info!("seeded {n} baseline interpretations");
                }
            }
        }
        Err(e) => tracing::warn!("baseline parse failed: {e}"),
    }

    // ── GTK application ───────────────────────────────────────────────────────
    let app = RelmApp::new("net.zodia.app");
    apply_css();
    app.run_async::<AppModel>(AppInit { config, store });
}

fn apply_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(CSS);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().expect("no display"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

const CSS: &str = r#"
/* ── typography ───────────────────────────────────────────────────────────── */
window {
    font-family: "Inter", "Noto Sans", sans-serif;
    background-color: #0e0e11;
    color: #e8e4f0;
}

/* ── setup page ─────────────────────────────────────────────────────────────*/
.setup-title {
    font-size: 28px;
    font-weight: 700;
    margin-bottom: 4px;
    color: #c8b8e8;
}
.setup-subtitle {
    font-size: 14px;
    color: #8880a0;
    margin-bottom: 12px;
}
.field-label {
    font-size: 13px;
    color: #9890a8;
    margin-right: 4px;
}
.spin-field {
    min-width: 64px;
}
.setup-error {
    color: #e05060;
    font-size: 13px;
    min-height: 18px;
}
.confirm-btn {
    padding: 10px 32px;
    font-size: 15px;
    font-weight: 600;
    margin-top: 8px;
    border-radius: 8px;
    background: linear-gradient(135deg, #5040a0, #8050c8);
    color: white;
    border: none;
}
.confirm-btn:hover { background: linear-gradient(135deg, #6050b0, #9060d8); }

/* ── panels ──────────────────────────────────────────────────────────────── */
.chart-panel, .peers-panel {
    background-color: #0e0e11;
}
.panel-header {
    padding: 14px 14px 10px 14px;
    background-color: #141418;
    border-bottom: 1px solid #2a2835;
}
.panel-title {
    font-size: 14px;
    font-weight: 600;
    color: #c8b8e8;
    letter-spacing: 0.04em;
    text-transform: uppercase;
}
.section-label {
    font-size: 11px;
    font-weight: 600;
    color: #6860a0;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    margin-bottom: 2px;
}
.node-id {
    font-size: 11px;
    color: #504870;
    font-family: monospace;
}
.aspect-list {
    font-family: monospace;
    font-size: 13px;
    line-height: 1.7;
    color: #d0c8e8;
}

/* ── peers panel ─────────────────────────────────────────────────────────── */
.peer-count {
    font-size: 12px;
    color: #8080c0;
}
.hint-label {
    font-size: 12px;
    color: #5c5878;
    line-height: 1.5;
}
.peer-list {
    background-color: transparent;
}
.peer-list row {
    border-bottom: 1px solid #1c1a28;
    background-color: transparent;
}
.peer-list row:hover { background-color: #1a1828; }
.peer-dot {
    font-size: 20px;
    color: #6050a8;
    min-width: 28px;
}
.aspects-line {
    font-size: 15px;
    font-family: monospace;
    color: #c8b8e8;
    letter-spacing: 0.05em;
}
.peer-meta {
    font-size: 12px;
    color: #706888;
}
.connect-btn {
    font-size: 12px;
    padding: 6px 14px;
    border-radius: 6px;
    background-color: #2a2840;
    color: #a090d0;
    border: 1px solid #3a3858;
}
.connect-btn:hover {
    background-color: #3a3860;
    color: #c0b0e8;
}
.call-btn {
    font-size: 14px;
    padding: 6px 10px;
    border-radius: 6px;
    background-color: #1e2840;
    color: #7090d0;
    border: 1px solid #2a3858;
}
.call-btn:hover {
    background-color: #2a3860;
    color: #90b0f0;
}

/* ── call bar ────────────────────────────────────────────────────────────── */
.call-bar {
    background-color: #12111c;
    border-top: 1px solid #2a2835;
    padding: 6px 0;
}
.call-status {
    font-size: 13px;
    color: #a090d0;
}
"#;
