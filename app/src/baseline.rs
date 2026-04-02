//! Fetch and cache the `baseline_aspects.toml` published as a GitHub release
//! asset.  Called once at startup in `main.rs` before the GTK loop starts.
//!
//! Lookup order:
//!   1. Local cache  — `$XDG_DATA_HOME/zodia/baseline_aspects.toml`
//!   2. GitHub       — latest release asset with that name
//!   3. Bundled      — `app/assets/baseline_aspects.toml` compiled into the binary

use std::path::Path;
use serde::Deserialize;
use tracing::{info, warn};

/// Set this to your actual GitHub `owner/repo` slug before publishing.
const GITHUB_REPO: &str = "owner/zodia";

const ASSET_NAME: &str = "baseline_aspects.toml";
const UA: &str = concat!("zodia/", env!("CARGO_PKG_VERSION"));

/// Compiled-in fallback — always available, updated on each release build.
const BUNDLED: &str = include_str!("../assets/baseline_aspects.toml");

/// Return the TOML text of the baseline interpretations, or `None` if
/// unavailable both locally and over the network.
pub fn load(data_dir: &Path) -> Option<String> {
    let cache = data_dir.join(ASSET_NAME);

    if cache.exists() {
        info!(path = %cache.display(), "loading cached baseline");
        return std::fs::read_to_string(&cache).ok();
    }

    info!("no cached baseline; fetching from GitHub releases");
    match fetch_latest() {
        Ok(toml) => {
            if let Err(e) = std::fs::write(&cache, &toml) {
                warn!("could not cache baseline to {}: {e}", cache.display());
            }
            Some(toml)
        }
        Err(e) => {
            warn!("could not fetch baseline: {e}; using bundled fallback");
            Some(BUNDLED.to_string())
        }
    }
}

// ── GitHub Releases API ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets:   Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name:                 String,
    browser_download_url: String,
}

fn fetch_latest() -> Result<String, Box<dyn std::error::Error>> {
    let url = format!(
        "https://api.github.com/repos/{GITHUB_REPO}/releases/latest"
    );
    let release: Release = ureq::get(&url)
        .set("User-Agent", UA)
        .set("Accept", "application/vnd.github+json")
        .call()?
        .into_json()?;

    let asset = release.assets.iter()
        .find(|a| a.name == ASSET_NAME)
        .ok_or_else(|| format!(
            "{ASSET_NAME} not found in release {}", release.tag_name
        ))?;

    info!(tag = %release.tag_name, "downloading {ASSET_NAME}");
    let toml = ureq::get(&asset.browser_download_url)
        .set("User-Agent", UA)
        .call()?
        .into_string()?;

    Ok(toml)
}
