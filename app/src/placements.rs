//! Chart placement items — sign + house placements as aggregate rows.
//!
//! Each planet emits one `AspectItem` whose keys carry both the sign and (for
//! non-stub charts) the house InterpKeys.  Angles (Ascendant + Midheaven) emit
//! one `AspectItem` each carrying a single sign key.
//!
//! The detail page synthesizes a "Combined" tab when 2+ keys are present.

use zodia_core::{Angle, Chart, InterpKey, Planet};

use crate::aspect_list::{AspectItem, KeyEntry};
use crate::util::{lon_to_sign_deg, sign_glyph};

/// One row per planet (sign + house keys when houses are real) and one row per
/// angle (sign key only).  Returned in this order: planets first, then ASC, MC.
pub fn placement_items(chart: &Chart) -> Vec<AspectItem> {
    let is_stub = chart.houses.cusps.iter().all(|&c| c == 0.0);
    let mut items = Vec::new();

    // ── planets ───────────────────────────────────────────────────────────────
    for &planet in Planet::all() {
        let Some(lon) = chart.positions.get(planet) else { continue };
        let (sign_idx, deg_str) = lon_to_sign_deg(lon);
        let sign_key = InterpKey::PlacementSign { planet, sign: sign_idx };

        let mut keys = vec![KeyEntry { label: "Sign".to_string(), key: sign_key.clone() }];
        let (title, symbol_line) = if is_stub {
            (sign_key.plain_name(),
             format!("{} {}", planet.symbol(), sign_glyph(sign_idx)))
        } else {
            let house = chart.houses.house_of(lon);
            let house_key = InterpKey::PlacementHouse { planet, house };
            keys.push(KeyEntry { label: "House".to_string(), key: house_key });
            (format!("{} · House {house}", sign_key.plain_name()),
             format!("{} {} ⌂{}", planet.symbol(), sign_glyph(sign_idx), house))
        };

        items.push(AspectItem {
            keys,
            title,
            symbol_line,
            meta_line: Some(deg_str),
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
        meta_line:   Some(asc_deg),
        transit_context: None,
    });

    let (mc_sign, mc_deg) = lon_to_sign_deg(chart.houses.midheaven);
    let mc_key = InterpKey::PlacementAngle { angle: Angle::Midheaven, sign: mc_sign };
    items.push(AspectItem {
        keys:        vec![KeyEntry { label: "Sign".to_string(), key: mc_key.clone() }],
        title:       mc_key.plain_name(),
        symbol_line: format!("MC {}", sign_glyph(mc_sign)),
        meta_line:   Some(mc_deg),
        transit_context: None,
    });

    items
}
