//! Chart placement items — sign + house placements as aggregate rows.
//!
//! Each planet emits one `AspectItem` with a single `InterpKey::Placement`
//! key carrying both sign and (for non-stub charts) house together — one
//! reading per planet, not two. Angles (Ascendant + Midheaven) emit one
//! `AspectItem` each carrying a single sign key (angles have no house).

use zodia_core::{Angle, Chart, InterpKey, Planet};

use crate::aspect_list::{AspectItem, KeyEntry};
use crate::util::{lon_to_sign_deg, sign_glyph};

/// One row per planet (sign + house together) and one row per angle (sign
/// only).  Returned in this order: planets first, then ASC, MC.
pub fn placement_items(chart: &Chart) -> Vec<AspectItem> {
    let is_stub = chart.houses.cusps.iter().all(|&c| c == 0.0);
    let mut items = Vec::new();

    // ── planets ───────────────────────────────────────────────────────────────
    for &planet in Planet::all() {
        let Some(lon) = chart.positions.get(planet) else { continue };
        let (sign_idx, deg_str) = lon_to_sign_deg(lon);
        // ℞ is the classical astrological retrograde glyph — shown next to
        // any planet whose apparent motion is currently backward from
        // Earth's vantage point (never Sun/Moon, see is_retrograde's doc).
        let retro_suffix = if zodia_core::is_retrograde(planet, chart.birth.jdn) { " ℞" } else { "" };

        let house = if is_stub { None } else { Some(chart.houses.house_of(lon)) };
        let key = InterpKey::Placement { planet, sign: sign_idx, house };
        // "{Planet} in {Sign}" without the house, for the row title's
        // leading segment — built from a house-less twin key rather than
        // `key.plain_name()` since that inlines house as "…, House N",
        // not this row's own "… · House N{retro}" formatting.
        let sign_only_name = InterpKey::Placement { planet, sign: sign_idx, house: None }.plain_name();
        let (title, symbol_line) = match house {
            Some(house) => (
                format!("{sign_only_name} · House {house}{retro_suffix}"),
                format!("{} {} ⌂{}{retro_suffix}", planet.symbol(), sign_glyph(sign_idx), house),
            ),
            None => (
                format!("{sign_only_name}{retro_suffix}"),
                format!("{} {}{retro_suffix}", planet.symbol(), sign_glyph(sign_idx)),
            ),
        };

        items.push(AspectItem {
            keys: vec![KeyEntry { label: "Placement".to_string(), key }],
            title,
            symbol_line,
            meta_line: Some(critical_degree_meta(lon, deg_str)),
            transit_context: None,
        });
    }

    // ── angles (Ascendant + Midheaven) ────────────────────────────────────────
    let (asc_sign, asc_deg) = lon_to_sign_deg(chart.houses.ascendant);
    let asc_key = InterpKey::PlacementAngle { angle: Angle::Ascendant, sign: asc_sign };
    items.push(AspectItem {
        keys:        vec![KeyEntry { label: "Sign".to_string(), key: asc_key.clone() }],
        title:       asc_key.plain_name(),
        symbol_line: format!("ASC {}", sign_glyph(asc_sign)),
        meta_line:   Some(critical_degree_meta(chart.houses.ascendant, asc_deg)),
        transit_context: None,
    });

    let (mc_sign, mc_deg) = lon_to_sign_deg(chart.houses.midheaven);
    let mc_key = InterpKey::PlacementAngle { angle: Angle::Midheaven, sign: mc_sign };
    items.push(AspectItem {
        keys:        vec![KeyEntry { label: "Sign".to_string(), key: mc_key.clone() }],
        title:       mc_key.plain_name(),
        symbol_line: format!("MC {}", sign_glyph(mc_sign)),
        meta_line:   Some(critical_degree_meta(chart.houses.midheaven, mc_deg)),
        transit_context: None,
    });

    items
}

/// Appends a "critical degree" note to a placement's degree label when it
/// falls in the last degree of its sign (29°) — a well-known, unambiguous
/// astrological concept that previously had nowhere to show at all.
fn critical_degree_meta(lon: f64, deg_str: String) -> String {
    if zodia_core::is_critical_degree(lon) {
        format!("{deg_str} · critical degree")
    } else {
        deg_str
    }
}
