//! Display helpers — plain-English aspect cards, glyphs, peer aspect scanning.

use zodia_core::{
    angular_separation, Aspect, AspectKind, HouseTransit, InterpKey, Planet, PlanetPositions,
    TransitAspect,
};
use zodia_store::ZodiaStore;

// ── zodiac signs ──────────────────────────────────────────────────────────────

/// Zodiac sign glyph from a solar month index (0 = Aries … 11 = Pisces).
pub fn sign_glyph(solar_month: u8) -> &'static str {
    const SIGNS: [&str; 12] = [
        "♈", "♉", "♊", "♋", "♌", "♍", "♎", "♏", "♐", "♑", "♒", "♓",
    ];
    SIGNS.get(solar_month as usize % 12).copied().unwrap_or("?")
}

// ── aspect card formatters ────────────────────────────────────────────────────

/// Multi-line card for a natal aspect.
///
/// ```text
/// Moon trine Venus  ·  ☽△♀  orb 2.3°
/// Warmth is given easily. Home and beauty feel like the same thing…
/// ```
pub fn format_aspect_card(a: &Aspect, store: &ZodiaStore) -> String {
    let key = InterpKey::from_natal(a);
    let plain = key.plain_name();
    let header = format!(
        "{}  ·  {}{}{}  orb {:.1}°",
        plain, a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb
    );
    match store.top_body(&key) {
        Ok(Some(body)) => format!("{header}\n  {body}"),
        _ => header,
    }
}

/// Multi-line card for a transit aspect.
///
/// ```text
/// Saturn square Sun (transit)  ·  ♄□☉  orb 1.8°
/// The scaffolding is being stress-tested…
/// ```
pub fn format_transit_card(ta: &TransitAspect, store: &ZodiaStore) -> String {
    let key = ta.interp_key();
    let plain = key.plain_name();
    let header = format!(
        "{}  ·  {}{}natal {}  orb {:.1}°",
        plain, ta.transiting.symbol(), ta.kind.symbol(), ta.natal_body.symbol(), ta.orb
    );
    match store.top_body(&key) {
        Ok(Some(body)) => format!("{header}\n  {body}"),
        _ => header,
    }
}

/// Single line for a house transit (less to say; house ingresses are context not drama).
///
/// ```text
/// Saturn in house 7  ♄  →  ⌂7
/// Relationships are being held to account…
/// ```
pub fn format_house_transit_card(ht: &HouseTransit, store: &ZodiaStore) -> String {
    let key = ht.interp_key();
    let plain = key.plain_name();
    let header = format!("{}  {}", plain, ht.transiting.symbol());
    match store.top_body(&key) {
        Ok(Some(body)) => format!("{header}\n  {body}"),
        _ => header,
    }
}

// ── peer approximate aspects ──────────────────────────────────────────────────

/// Glyph strings for probable aspects between a peer's approximate Sun
/// (derived from `solar_month`) and each of our natal planets.
///
/// Orbs are widened +5° to account for the ±15° imprecision in solar_month.
pub fn approximate_aspects(solar_month: u8, natal: &PlanetPositions) -> Vec<String> {
    let peer_sun = solar_month as f64 * 30.0 + 15.0;
    let mut out = Vec::new();
    for &planet in Planet::all() {
        let Some(natal_lon) = natal.get(planet) else { continue };
        let sep = angular_separation(peer_sun, natal_lon);
        if let Some(kind) = detect_wide(sep) {
            out.push(format!("☉{}{}", kind.symbol(), planet.symbol()));
        }
    }
    out
}

fn detect_wide(sep: f64) -> Option<AspectKind> {
    const ALL: &[AspectKind] = &[
        AspectKind::Conjunction, AspectKind::SemiSextile, AspectKind::Sextile,
        AspectKind::Square, AspectKind::Trine, AspectKind::Quincunx, AspectKind::Opposition,
    ];
    ALL.iter()
        .find(|&&kind| (sep - kind.angle()).abs() <= kind.default_orb() + 5.0)
        .copied()
}
