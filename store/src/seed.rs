//! Baseline seed data — TOML shape and parser.
//!
//! The seeding *methods* live on `ZodiaStore` in `lib.rs`; this module only
//! owns the data type so that consumers don't need to pull in the TOML parser.

use std::collections::HashMap;
use serde::Deserialize;
use zodia_core::InterpKey;
use crate::StoreError;

/// Parsed contents of `baseline_aspects.toml`.
///
/// TOML shape:
/// ```toml
/// [natal]
/// "moon_trine_venus" = "Warmth is given easily…"
///
/// [transit]
/// "saturn_square_sun" = "The scaffolding is being stress-tested…"
///
/// [house_transit]
/// "saturn:7" = "Relationships are being held to account…"
///
/// [placement_sign]
/// "jupiter:virgo" = "Practical optimism — abundance through diligent care…"
///
/// [placement_house]
/// "jupiter:9" = "Belief expands through travel, study, and far horizons…"
/// ```
///
/// Keys within each section are the *discriminator* portion of the sig
/// (without the leading prefix that `InterpKey::to_sig()` adds).
#[derive(Debug, Deserialize)]
pub struct BaselineData {
    #[serde(default)]
    pub natal:           HashMap<String, String>,
    #[serde(default)]
    pub synastry:        HashMap<String, String>,
    #[serde(default)]
    pub transit:         HashMap<String, String>,
    #[serde(default)]
    pub house_transit:   HashMap<String, String>,
    #[serde(default)]
    pub placement_sign:  HashMap<String, String>,
    #[serde(default)]
    pub placement_house: HashMap<String, String>,
}

impl BaselineData {
    /// Parse from raw TOML text (typically via `include_str!`).
    pub fn from_toml(src: &str) -> Result<Self, StoreError> {
        toml::from_str(src).map_err(|e| StoreError::Seed(e.to_string()))
    }
}

// ── in-memory baseline store ──────────────────────────────────────────────────

/// Read-only in-memory store for baseline interpretations.
///
/// Keyed by the full canonical sig string that `InterpKey::to_sig()` produces
/// (e.g. `"natal:jupiter_trine_venus"`), so every lookup is a single
/// `HashMap::get` with no allocation.
pub struct BaselineStore {
    map: HashMap<String, String>,
}

impl BaselineStore {
    /// Build from parsed TOML data. O(n), allocates once at startup.
    pub fn from_data(data: &BaselineData) -> Self {
        let mut map = HashMap::new();
        for (k, v) in &data.natal           { map.insert(format!("natal:{k}"),           v.clone()); }
        for (k, v) in &data.synastry        { map.insert(format!("synastry:{k}"),        v.clone()); }
        for (k, v) in &data.transit         { map.insert(format!("transit:{k}"),         v.clone()); }
        for (k, v) in &data.house_transit   { map.insert(format!("house_transit:{k}"),   v.clone()); }
        for (k, v) in &data.placement_sign  { map.insert(format!("placement_sign:{k}"),  v.clone()); }
        for (k, v) in &data.placement_house { map.insert(format!("placement_house:{k}"), v.clone()); }
        Self { map }
    }

    /// Empty store — used as a safe fallback when the TOML fails to load.
    pub fn empty() -> Self {
        Self { map: HashMap::new() }
    }

    pub fn len(&self) -> usize { self.map.len() }
    pub fn is_empty(&self) -> bool { self.map.is_empty() }

    /// Borrow the baseline text for a key, if present.
    pub fn lookup(&self, key: &InterpKey) -> Option<&str> {
        self.map.get(&key.to_sig()).map(String::as_str)
    }
}
