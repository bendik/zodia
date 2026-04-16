//! Baseline interpretations compiled into the binary.

/// Bundled baseline — compiled in at build time, always available.
const BUNDLED: &str = include_str!("../assets/baseline_aspects.toml");

/// Return the bundled baseline TOML text.
pub fn load() -> &'static str {
    BUNDLED
}
