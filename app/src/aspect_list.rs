//! Aspect item constructors — data layer for the aspect views.
//!
//! Converts core aspect types into `AspectItem` slices consumed by
//! `aspect_view::AspectView`.  No GTK dependency; pure data.

use zodia_core::{
    Aspect, HouseTransit, InterpKey, PlanetPositions, SynastryAspect, TransitAspect,
    jdn_to_display_date, transit_window,
};

// ── row data ──────────────────────────────────────────────────────────────────

/// Data for a single aspect list row.
pub struct AspectItem {
    pub key: InterpKey,
    /// Compact glyph string, e.g. "☽△♀  orb 2.3°" or "♄  →  ⌂7"
    pub glyph_suffix: String,
    /// Human-readable date range string for transit aspects, e.g.
    /// "Active: 8 Apr – 16 Apr 2026".  `None` for natal/synastry items.
    pub transit_context: Option<String>,
}

pub fn natal_items(aspects: &[Aspect]) -> Vec<AspectItem> {
    aspects
        .iter()
        .map(|a| AspectItem {
            key: InterpKey::from_natal(a),
            glyph_suffix: format!(
                "{} {} {}  orb {:.1}°",
                a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb
            ),
            transit_context: None,
        })
        .collect()
}

pub fn transit_items(
    transits: &[TransitAspect],
    house_transits: &[HouseTransit],
    natal_positions: &PlanetPositions,
    transit_jdn: f64,
) -> Vec<AspectItem> {
    let mut items: Vec<AspectItem> = transits
        .iter()
        .map(|ta| {
            let window_str = natal_positions
                .get(ta.natal_body)
                .map(|natal_lon| {
                    let (start, end) =
                        transit_window(ta.transiting, natal_lon, ta.kind, transit_jdn);
                    let s = jdn_to_display_date(start);
                    let e = jdn_to_display_date(end);
                    if s == e {
                        format!("Active: {s}")
                    } else {
                        format!("Active: {s} – {e}")
                    }
                });
            AspectItem {
                key: ta.interp_key(),
                glyph_suffix: format!(
                    "{} {} natal {}  orb {:.1}°",
                    ta.transiting.symbol(), ta.kind.symbol(), ta.natal_body.symbol(), ta.orb
                ),
                transit_context: window_str,
            }
        })
        .collect();

    let as_of = jdn_to_display_date(transit_jdn);
    for ht in house_transits {
        items.push(AspectItem {
            key: ht.interp_key(),
            glyph_suffix: format!("{}  →  ⌂{}", ht.transiting.symbol(), ht.house),
            transit_context: Some(format!("As of {as_of}")),
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
                "{} {} {}  orb {:.1}°",
                a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb
            ),
            transit_context: None,
        })
        .collect()
}
