//! Chart placement widget — planets, ASC, and MC in sign and house.

use libadwaita as adw;
use libadwaita::prelude::*;
use zodia_core::{Chart, Planet};

use crate::util::{lon_to_sign_deg, sign_glyph, sign_name};

/// Build a `PreferencesGroup` listing every planet's sign placement and house,
/// followed by the Ascendant and Midheaven.
///
/// If the house system is a stub (all cusps zero — happens when the geohash is
/// too short to resolve coordinates), house numbers are omitted.
pub fn build_placements_group(chart: &Chart) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Placements");

    let is_stub = chart.houses.cusps.iter().all(|&c| c == 0.0);

    // ── planets ───────────────────────────────────────────────────────────────

    for &planet in Planet::all() {
        let Some(lon) = chart.positions.get(planet) else { continue };
        let (sign_idx, deg_str) = lon_to_sign_deg(lon);
        let house = chart.houses.house_of(lon);

        let row = adw::ActionRow::new();
        row.set_title(&format!(
            "{}  {}  in  {}  {}",
            planet.symbol(),
            capitalize(planet.name()),
            sign_glyph(sign_idx),
            sign_name(sign_idx),
        ));
        row.set_subtitle(&if is_stub {
            deg_str
        } else {
            format!("{}  ·  House {house}", deg_str)
        });
        group.add(&row);
    }

    // ── angles ────────────────────────────────────────────────────────────────

    let (asc_sign, asc_deg) = lon_to_sign_deg(chart.houses.ascendant);
    let asc_row = adw::ActionRow::new();
    asc_row.set_title(&format!(
        "Ascendant  in  {}  {}",
        sign_glyph(asc_sign),
        sign_name(asc_sign),
    ));
    asc_row.set_subtitle(&format!("{}  ·  rising sign", asc_deg));
    group.add(&asc_row);

    let (mc_sign, mc_deg) = lon_to_sign_deg(chart.houses.midheaven);
    let mc_row = adw::ActionRow::new();
    mc_row.set_title(&format!(
        "Midheaven  in  {}  {}",
        sign_glyph(mc_sign),
        sign_name(mc_sign),
    ));
    mc_row.set_subtitle(&format!("{}  ·  culminating", mc_deg));
    group.add(&mc_row);

    group
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None    => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}
