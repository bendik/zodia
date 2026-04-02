//! Baseline seed data — TOML shape and parser.
//!
//! The seeding *methods* live on `ZodiaStore` in `lib.rs`; this module only
//! owns the data type so that consumers don't need to pull in the TOML parser.

use std::collections::HashMap;
use serde::Deserialize;
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
/// ```
///
/// Keys within each section are the *discriminator* portion of the sig
/// (without the leading `"natal:"` / `"transit:"` / `"house_transit:"` prefix
/// that `InterpKey::to_sig()` adds).
#[derive(Debug, Deserialize)]
pub struct BaselineData {
    #[serde(default)]
    pub natal:         HashMap<String, String>,
    #[serde(default)]
    pub synastry:      HashMap<String, String>,
    #[serde(default)]
    pub transit:       HashMap<String, String>,
    #[serde(default)]
    pub house_transit: HashMap<String, String>,
}

impl BaselineData {
    /// Parse from raw TOML text (typically via `include_str!`).
    pub fn from_toml(src: &str) -> Result<Self, StoreError> {
        toml::from_str(src).map_err(|e| StoreError::Seed(e.to_string()))
    }
}
