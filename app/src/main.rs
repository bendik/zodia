//! Zodia — p2p astrological encounter network.
//!
//! Entry point: initialise tracing, load or create local config, then hand
//! control to the Relm4 application loop.

mod app;
mod aspect_list;
mod aspect_view;
mod baseline;
mod peer_list;
mod peer_page;
mod placements;
mod util;

use app::{AppInit, AppModel};
use relm4::RelmApp;
use zodia_config::LocalConfig;
use zodia_store::{ZodiaStore, seed::BaselineData};

fn main() {
    // ── tracing ───────────────────────────────────────────────────────────────
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

    // ── local config ─────────────────────────────────────────────────────────
    let config = match LocalConfig::load_or_create() {
        Ok(c) => c,
        Err(e) => { eprintln!("fatal: could not load config: {e}"); std::process::exit(1); }
    };

    // ── store ─────────────────────────────────────────────────────────────────
    let store_path = config.data_dir().join("interpretations.db");
    let store = match ZodiaStore::open(&store_path) {
        Ok(s) => s,
        Err(e) => { eprintln!("fatal: could not open store: {e}"); std::process::exit(1); }
    };
    if let Some(toml) = baseline::load(config.data_dir()) {
        match BaselineData::from_toml(&toml) {
            Ok(b) => {
                if let Ok(n) = store.seed_if_empty(&b) {
                    if n > 0 { tracing::info!("seeded {n} baseline interpretations"); }
                }
            }
            Err(e) => tracing::warn!("baseline parse failed: {e}"),
        }
    } else {
        tracing::info!("no baseline available; store starts empty");
    }

    // ── Bundled icons ─────────────────────────────────────────────────────────
    // Register SVG icons compiled into the binary at build time so they render
    // on all platforms (macOS has no system Adwaita icon theme).
    gio::resources_register_include!("icons.gresource")
        .expect("failed to register bundled icon resources");

    // ── GTK / ADW application ─────────────────────────────────────────────────
    let app = RelmApp::new("net.zodia.app");

    // Add the bundled icon resource path to the default icon theme so GTK
    // resolves our icons on every platform including macOS.
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::IconTheme::for_display(&display)
            .add_resource_path("/net/zodia/app/icons");
    }

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
/* ── peer list rows ────────────────────────────────────────────────────────── */
.peer-dot {
    font-size: 20px;
    min-width: 28px;
}
.aspects-line {
    font-size: 15px;
    font-family: monospace;
    letter-spacing: 0.05em;
}
.peer-meta {
    font-size: 12px;
}
.connect-btn {
    font-size: 12px;
    padding: 4px 12px;
}
.call-btn {
    font-size: 14px;
    padding: 4px 8px;
}

/* ── chart content ──────────────────────────────────────────────────────────── */
.aspect-list {
    font-family: monospace;
    font-size: 13px;
    line-height: 1.7;
}

/* ── node id ────────────────────────────────────────────────────────────────── */
.node-id {
    font-family: monospace;
    font-size: 11px;
}
"#;
