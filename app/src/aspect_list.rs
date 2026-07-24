//! Interpretation row data layer for `AspectView`.
//!
//! The `AspectItem` type is misleading by name now — it carries any aggregate
//! of `InterpKey`s a row should expose, not just aspects.  Placements feed it
//! too (one row per planet with sign + house keys).

use zodia_core::{Aspect, InterpKey, SynastryAspect};

// ── row data ──────────────────────────────────────────────────────────────────

/// A single key on a row, paired with the label used as its detail-page tab title.
#[derive(Clone, Debug)]
pub struct KeyEntry {
    pub label: String,
    pub key:   InterpKey,
}

/// Data for a single aggregate interpretation row.
pub struct AspectItem {
    /// One or more interpretation keys for this row.  Aspects emit one entry;
    /// placements may emit two (sign + house).
    pub keys:            Vec<KeyEntry>,
    /// Plain-English row title shown by the row, e.g. "Jupiter trine Venus" or
    /// "Jupiter in Virgo · House 9".
    pub title:           String,
    /// Top suffix line — compact glyph string, e.g. "☽ △ ♀" or "♃ ♍ ⌂9".
    pub symbol_line:     String,
    /// Bottom suffix line — orb / metadata.  `None` when there's nothing useful.
    pub meta_line:       Option<String>,
    /// Human-readable date range for transit aspects.  `None` for natal /
    /// synastry / placement items.
    pub transit_context: Option<String>,
}

pub fn natal_items(aspects: &[Aspect]) -> Vec<AspectItem> {
    aspects.iter().map(|a| {
        let key = InterpKey::from_natal(a);
        AspectItem {
            keys:        vec![KeyEntry { label: "Aspect".to_string(), key: key.clone() }],
            title:       key.plain_name(),
            symbol_line: format!(
                "{} {} {}",
                a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(),
            ),
            meta_line:       Some(format!("orb {:.1}°", a.orb)),
            transit_context: None,
        }
    }).collect()
}

pub fn synastry_items(aspects: &[SynastryAspect]) -> Vec<AspectItem> {
    aspects.iter().map(|a| {
        let key = InterpKey::from_synastry(a);
        AspectItem {
            keys:        vec![KeyEntry { label: "Aspect".to_string(), key: key.clone() }],
            title:       key.plain_name(),
            symbol_line: format!(
                "{} {} {}",
                a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(),
            ),
            meta_line:       Some(format!("orb {:.1}°", a.orb)),
            transit_context: None,
        }
    }).collect()
}
