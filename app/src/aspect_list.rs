//! Interpretation row data layer for `AspectView`.
//!
//! The `AspectItem` type is misleading by name now — it carries any aggregate
//! of `InterpKey`s a row should expose, not just aspects.  Placements feed it
//! too (one row per planet with sign + house keys).

use zodia_core::{
    Aspect, HouseTransit, InterpKey, PlanetPositions, SynastryAspect, TransitAspect,
    jdn_to_display_date, transit_window,
};

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

pub fn transit_items(
    transits: &[TransitAspect],
    house_transits: &[HouseTransit],
    natal_positions: &PlanetPositions,
    transit_jdn: f64,
) -> Vec<AspectItem> {
    let mut items: Vec<AspectItem> = transits.iter().map(|ta| {
        let window_str = natal_positions
            .get(ta.natal_body)
            .map(|natal_lon| {
                let (start, end) = transit_window(ta.transiting, natal_lon, ta.kind, transit_jdn);
                let s = jdn_to_display_date(start);
                let e = jdn_to_display_date(end);
                if s == e { format!("Active: {s}") } else { format!("Active: {s} – {e}") }
            });
        let key = ta.interp_key();
        AspectItem {
            keys:        vec![KeyEntry { label: "Aspect".to_string(), key: key.clone() }],
            title:       key.plain_name(),
            symbol_line: format!(
                "{} {} natal {}",
                ta.transiting.symbol(), ta.kind.symbol(), ta.natal_body.symbol(),
            ),
            meta_line:       Some(format!("orb {:.1}°", ta.orb)),
            transit_context: window_str,
        }
    }).collect();

    let as_of = jdn_to_display_date(transit_jdn);
    for ht in house_transits {
        let key = ht.interp_key();
        items.push(AspectItem {
            keys:        vec![KeyEntry { label: "House transit".to_string(), key: key.clone() }],
            title:       key.plain_name(),
            symbol_line: format!("{} → ⌂{}", ht.transiting.symbol(), ht.house),
            meta_line:   None,
            transit_context: Some(format!("As of {as_of}")),
        });
    }
    items
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
