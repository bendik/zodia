//! Chart placement items — sign + house placements as interpretation rows.
//!
//! Returns a `Vec<AspectItem>` so the natal `AspectView` can feed them straight
//! into its `InterpRow` factory alongside aspect rows.  Each planet contributes
//! up to two rows: one `PlacementSign` (always) and one `PlacementHouse` (when
//! the chart has resolvable houses).

use zodia_core::{Chart, InterpKey, Planet};

use crate::aspect_list::AspectItem;
use crate::util::{lon_to_sign_deg, sign_glyph};

/// One row per (planet, sign) and one per (planet, house) when houses are real.
pub fn placement_items(chart: &Chart) -> Vec<AspectItem> {
    let is_stub = chart.houses.cusps.iter().all(|&c| c == 0.0);
    let mut items = Vec::new();

    for &planet in Planet::all() {
        let Some(lon) = chart.positions.get(planet) else { continue };
        let (sign_idx, deg_str) = lon_to_sign_deg(lon);

        // PlacementSign row.
        let sign_key = InterpKey::PlacementSign { planet, sign: sign_idx };
        items.push(AspectItem {
            key:         sign_key,
            symbol_line: format!("{} {}", planet.symbol(), sign_glyph(sign_idx)),
            meta_line:   Some(deg_str),
            transit_context: None,
        });

        // PlacementHouse row (skip for stub charts).
        if !is_stub {
            let house = chart.houses.house_of(lon);
            let house_key = InterpKey::PlacementHouse { planet, house };
            items.push(AspectItem {
                key:         house_key,
                symbol_line: format!("{} ⌂{}", planet.symbol(), house),
                meta_line:   None,
                transit_context: None,
            });
        }
    }

    items
}
