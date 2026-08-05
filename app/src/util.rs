//! Display helpers — plain-English aspect cards, glyphs, peer aspect scanning.

use zodia_core::{
    angular_separation, Aspect, AspectKind, HouseTransit, InterpKey, Planet, PlanetPositions,
    TransitAspect,
};
use zodia_store::ZodiaStore;

// ── zodiac signs ──────────────────────────────────────────────────────────────

/// Zodiac sign glyph from a solar month index (0 = Aries … 11 = Pisces).
///
/// Each glyph is followed by U+FE0E (variation selector-15) to force text
/// presentation on platforms (macOS) that would otherwise render them as emoji.
pub fn sign_glyph(solar_month: u8) -> &'static str {
    const SIGNS: [&str; 12] = [
        "♈\u{FE0E}", "♉\u{FE0E}", "♊\u{FE0E}", "♋\u{FE0E}",
        "♌\u{FE0E}", "♍\u{FE0E}", "♎\u{FE0E}", "♏\u{FE0E}",
        "♐\u{FE0E}", "♑\u{FE0E}", "♒\u{FE0E}", "♓\u{FE0E}",
    ];
    SIGNS.get(solar_month as usize % 12).copied().unwrap_or("?")
}

/// Ecliptic longitude → (sign_index 0–11, formatted degree string "17°23′").
pub fn lon_to_sign_deg(lon: f64) -> (u8, String) {
    let lon = lon.rem_euclid(360.0);
    let sign_idx = (lon / 30.0).floor() as u8 % 12;
    let within   = lon % 30.0;
    let deg      = within.floor() as u32;
    let min      = ((within - deg as f64) * 60.0).round() as u32;
    (sign_idx, format!("{deg}°{min:02}′"))
}

// ── presence ──────────────────────────────────────────────────────────────────

/// "Last seen 5m ago" — style relative-time text for an offline peer's
/// direct-channel close timestamp. Takes `now` explicitly rather than
/// reading the clock itself so it stays a pure, deterministically
/// testable function; callers pass the real current unix time.
pub fn format_last_seen(now: u64, last_seen: u64) -> String {
    let diff = now.saturating_sub(last_seen);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 7 * 86_400 {
        format!("{}d ago", diff / 86_400)
    } else {
        "a while ago".to_string()
    }
}

// ── aspect card formatters ────────────────────────────────────────────────────

/// Multi-line card for a natal aspect — kept for future synastry/export views.
#[allow(dead_code)]
///
/// ```text
/// Moon trine Venus  ·  ☽△♀  orb 2.3°
/// Warmth is given easily. Home and beauty feel like the same thing…
/// ```
pub async fn format_aspect_card(a: &Aspect, store: &ZodiaStore) -> String {
    let key = InterpKey::from_natal(a);
    let plain = key.plain_name();
    let header = format!(
        "{}  ·  {}{}{}  orb {:.1}°",
        plain, a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb
    );
    match store.top_body(&key).await {
        Ok(Some(body)) => format!("{header}\n  {body}"),
        _ => header,
    }
}

#[allow(dead_code)]
/// Multi-line card for a transit aspect.
///
/// ```text
/// Saturn square Sun (transit)  ·  ♄□☉  orb 1.8°
/// The scaffolding is being stress-tested…
/// ```
pub async fn format_transit_card(ta: &TransitAspect, store: &ZodiaStore) -> String {
    let key = ta.interp_key();
    let plain = key.plain_name();
    let header = format!(
        "{}  ·  {}{}natal {}  orb {:.1}°",
        plain, ta.transiting.symbol(), ta.kind.symbol(), ta.natal_body.symbol(), ta.orb
    );
    match store.top_body(&key).await {
        Ok(Some(body)) => format!("{header}\n  {body}"),
        _ => header,
    }
}

#[allow(dead_code)]
/// Single line for a house transit (less to say; house ingresses are context not drama).
///
/// ```text
/// Saturn in house 7  ♄  →  ⌂7
/// Relationships are being held to account…
/// ```
pub async fn format_house_transit_card(ht: &HouseTransit, store: &ZodiaStore) -> String {
    let key = ht.interp_key();
    let plain = key.plain_name();
    let header = format!("{}  {}", plain, ht.transiting.symbol());
    match store.top_body(&key).await {
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
            out.push(format!("☉ {}{}", kind.symbol(), planet.symbol()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn just_under_a_minute_reads_as_just_now() {
        assert_eq!(format_last_seen(1000, 1000), "just now");
        assert_eq!(format_last_seen(1059, 1000), "just now");
    }

    #[test]
    fn minutes_boundary_is_exact() {
        assert_eq!(format_last_seen(1060, 1000), "1m ago");
        assert_eq!(format_last_seen(1000 + 3599, 1000), "59m ago");
    }

    #[test]
    fn hours_boundary_is_exact() {
        assert_eq!(format_last_seen(1000 + 3600, 1000), "1h ago");
        assert_eq!(format_last_seen(1000 + 86_399, 1000), "23h ago");
    }

    #[test]
    fn days_boundary_is_exact() {
        assert_eq!(format_last_seen(1000 + 86_400, 1000), "1d ago");
        assert_eq!(format_last_seen(1000 + 7 * 86_400 - 1, 1000), "6d ago");
    }

    #[test]
    fn a_week_or_more_reads_as_a_while_ago() {
        assert_eq!(format_last_seen(1000 + 7 * 86_400, 1000), "a while ago");
    }

    #[test]
    fn clock_skew_never_underflows_or_shows_the_future() {
        // last_seen after "now" (system clock adjustment, restart race) —
        // must not panic on subtraction underflow or print a negative time.
        assert_eq!(format_last_seen(1000, 2000), "just now");
    }
}
