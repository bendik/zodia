//! Display helpers — glyphs, formatting, approximate aspect scanning.

use zodia_core::{angular_separation, Planet, PlanetPositions};

/// Zodiac sign glyph from a solar month index (0 = Aries … 11 = Pisces).
pub fn sign_glyph(solar_month: u8) -> &'static str {
    const SIGNS: [&str; 12] = [
        "♈", "♉", "♊", "♋", "♌", "♍", "♎", "♏", "♐", "♑", "♒", "♓",
    ];
    SIGNS.get(solar_month as usize % 12).copied().unwrap_or("?")
}

/// Build short glyph strings for probable aspects between a peer's approximate
/// Sun and every natal planet we hold.
///
/// `solar_month` → Sun ≈ `solar_month × 30 + 15` degrees.
/// Orbs are widened by 5° to account for the ±15° uncertainty.
pub fn approximate_aspects(solar_month: u8, natal: &PlanetPositions) -> Vec<String> {
    let peer_sun = solar_month as f64 * 30.0 + 15.0;
    let mut out = Vec::new();

    for &planet in Planet::all() {
        let Some(natal_lon) = natal.get(planet) else { continue };
        let sep = angular_separation(peer_sun, natal_lon);
        if let Some((kind, _orb)) = detect_aspect_wide(sep) {
            out.push(format!("☉{}{}", kind.symbol(), planet.symbol()));
        }
    }
    out
}

/// Aspect detection with +5° extra orb to accommodate solar_month imprecision.
fn detect_aspect_wide(sep: f64) -> Option<(zodia_core::AspectKind, f64)> {
    use zodia_core::AspectKind;
    const ALL: &[AspectKind] = &[
        AspectKind::Conjunction,
        AspectKind::SemiSextile,
        AspectKind::Sextile,
        AspectKind::Square,
        AspectKind::Trine,
        AspectKind::Quincunx,
        AspectKind::Opposition,
    ];
    ALL.iter().find_map(|&kind| {
        let orb = sep - kind.angle();
        (orb.abs() <= kind.default_orb() + 5.0).then_some((kind, orb))
    })
}

/// Format a natal aspect for the chart panel label.
pub fn format_natal_aspect(a: &zodia_core::Aspect) -> String {
    format!(
        "{} {} {}  {:.1}°",
        a.body_a.symbol(),
        a.kind.symbol(),
        a.body_b.symbol(),
        a.orb,
    )
}

/// Format a transit aspect for the transits panel label.
pub fn format_transit(ta: &zodia_core::TransitAspect) -> String {
    format!(
        "{} {} natal {}  {:.1}°",
        ta.transiting.symbol(),
        ta.kind.symbol(),
        ta.natal_body.symbol(),
        ta.orb,
    )
}

/// Format a house transit for display.
pub fn format_house_transit(ht: &zodia_core::HouseTransit) -> String {
    format!("{} in house {}", ht.transiting.symbol(), ht.house)
}
