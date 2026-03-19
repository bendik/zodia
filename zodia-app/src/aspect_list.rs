//! Aspect item constructors — data layer for the aspect views.
//!
//! Converts core aspect types into `AspectItem` slices consumed by
//! `aspect_view::AspectView`.  No GTK dependency; pure data.

use zodia_core::{Aspect, HouseTransit, InterpKey, SynastryAspect, TransitAspect};

// ── row data ──────────────────────────────────────────────────────────────────

/// Data for a single aspect list row.
pub struct AspectItem {
    pub key: InterpKey,
    /// Compact glyph string, e.g. "☽△♀  orb 2.3°" or "♄  →  ⌂7"
    pub glyph_suffix: String,
}

pub fn natal_items(aspects: &[Aspect]) -> Vec<AspectItem> {
    aspects
        .iter()
        .map(|a| AspectItem {
            key: InterpKey::from_natal(a),
            glyph_suffix: format!(
                "{}{}{}  orb {:.1}°",
                a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb
            ),
        })
        .collect()
}

pub fn transit_items(
    transits: &[TransitAspect],
    house_transits: &[HouseTransit],
) -> Vec<AspectItem> {
    let mut items: Vec<AspectItem> = transits
        .iter()
        .map(|ta| AspectItem {
            key: ta.interp_key(),
            glyph_suffix: format!(
                "{}{}natal {}  orb {:.1}°",
                ta.transiting.symbol(), ta.kind.symbol(), ta.natal_body.symbol(), ta.orb
            ),
        })
        .collect();

    for ht in house_transits {
        items.push(AspectItem {
            key: ht.interp_key(),
            glyph_suffix: format!("{}  →  ⌂{}", ht.transiting.symbol(), ht.house),
        });
    }
    items
}

pub fn synastry_items(aspects: &[SynastryAspect]) -> Vec<AspectItem> {
    aspects
        .iter()
        .map(|a| AspectItem {
            key: InterpKey::from_synastry(a),
            glyph_suffix: format!(
                "{}{}{}  orb {:.1}°",
                a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb
            ),
        })
        .collect()
}
