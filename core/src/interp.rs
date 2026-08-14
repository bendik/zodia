//! Interpretation keys — the structured identifiers that link astrological
//! phenomena to community-written text in the interpretation index.
//!
//! Every entry in the store has an `InterpKey` that answers "what is this
//! text describing?"  The key encodes both the *kind* of phenomenon (natal
//! aspect, synastry aspect, transit aspect, house transit) and its identity
//! (which planets/houses are involved).

use crate::aspects::{AspectKind, Aspect, SynastryAspect};
use crate::planet::Planet;
use serde::{Deserialize, Serialize};

/// Broad category of an interpretation — used for UI filtering and routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InterpKind {
    /// Aspect between two planets in a single natal chart
    Natal,
    /// Aspect between planets across two people's charts
    Synastry,
    /// Transiting planet making an aspect to a natal planet
    Transit,
    /// Aspect between two transiting bodies in the current sky (global)
    SkyAspect,
    /// Transiting planet occupying a natal house
    HouseTransit,
    /// Natal planet's sign + house placement together (e.g. "Jupiter in
    /// Virgo, House 9") — one key, one community reading, per planet.
    Placement,
    /// Sign placement of an angle (Ascendant or Midheaven)
    PlacementAngle,
}

/// Chart angle — Ascendant (rising) or Midheaven (culminating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Angle {
    Ascendant,
    Midheaven,
}

impl Angle {
    /// Lowercase short tag used in canonical sigs: `"asc"` / `"mc"`.
    pub fn tag(&self) -> &'static str {
        match self {
            Angle::Ascendant => "asc",
            Angle::Midheaven => "mc",
        }
    }
    /// Display name for plain_name and UI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Angle::Ascendant => "Ascendant",
            Angle::Midheaven => "Midheaven",
        }
    }
}

/// Fully qualified key for a community interpretation entry.
///
/// Canonical DB string examples:
/// - `"natal:jupiter_trine_venus"`
/// - `"synastry:jupiter_trine_venus"`
/// - `"transit:saturn_square_natal_venus"`
/// - `"house_transit:saturn:7"`
/// - `"placement:jupiter:virgo:9"` (sign + house together; house omitted —
///   `"placement:jupiter:virgo"` — for stub charts with no house data)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InterpKey {
    Natal {
        /// Sorted planet-pair + aspect, e.g. `"jupiter_trine_venus"`
        aspect_sig: String,
    },
    Synastry {
        aspect_sig: String,
    },
    Transit {
        transiting: Planet,
        natal_body: Planet,
        kind: AspectKind,
    },
    /// Aspect between two transiting bodies in the current sky.  Same shape
    /// as `Natal`'s `aspect_sig` (e.g. `"mars_conjunction_sun"`) but the
    /// interpretation is about the *sky right now*, not the natal chart.
    SkyAspect {
        aspect_sig: String,
    },
    HouseTransit {
        transiting: Planet,
        /// Natal house number (1–12)
        house: u8,
    },
    Placement {
        planet: Planet,
        /// Sign index 0..11 (Aries..Pisces).
        sign:   u8,
        /// Natal house number (1–12). `None` only for stub charts with no
        /// house system computed — every real chart has one.
        house:  Option<u8>,
    },
    PlacementAngle {
        angle: Angle,
        /// Sign index 0..11.
        sign:  u8,
    },
}

/// Lowercase sign name for sig encoding — `"aries"`..`"pisces"`.
pub fn sign_name_lower(idx: u8) -> &'static str {
    const NAMES: [&str; 12] = [
        "aries", "taurus", "gemini", "cancer",
        "leo", "virgo", "libra", "scorpio",
        "sagittarius", "capricorn", "aquarius", "pisces",
    ];
    NAMES.get(idx as usize % 12).copied().unwrap_or("?")
}

impl InterpKey {
    /// Canonical string written to the `interp_key` column in SQLite.
    pub fn to_sig(&self) -> String {
        match self {
            Self::Natal       { aspect_sig }  => format!("natal:{aspect_sig}"),
            Self::Synastry    { aspect_sig }  => format!("synastry:{aspect_sig}"),
            Self::SkyAspect   { aspect_sig }  => format!("sky:{aspect_sig}"),
            Self::Transit     { transiting, natal_body, kind } =>
                format!("transit:{}_{}_{}", transiting.name(), kind.name(), natal_body.name()),
            Self::HouseTransit { transiting, house } =>
                format!("house_transit:{}:{house}", transiting.name()),
            Self::Placement { planet, sign, house } => match house {
                Some(h) => format!("placement:{}:{}:{h}", planet.name(), sign_name_lower(*sign)),
                None    => format!("placement:{}:{}", planet.name(), sign_name_lower(*sign)),
            },
            Self::PlacementAngle { angle, sign } =>
                format!("placement_angle:{}:{}", angle.tag(), sign_name_lower(*sign)),
        }
    }

    pub fn kind(&self) -> InterpKind {
        match self {
            Self::Natal       { .. }       => InterpKind::Natal,
            Self::Synastry    { .. }       => InterpKind::Synastry,
            Self::SkyAspect   { .. }       => InterpKind::SkyAspect,
            Self::Transit     { .. }       => InterpKind::Transit,
            Self::HouseTransit { .. }      => InterpKind::HouseTransit,
            Self::Placement       { .. }    => InterpKind::Placement,
            Self::PlacementAngle { .. }    => InterpKind::PlacementAngle,
        }
    }

    pub fn from_natal(aspect: &Aspect) -> Self {
        Self::Natal { aspect_sig: aspect.sig() }
    }

    pub fn from_synastry(aspect: &SynastryAspect) -> Self {
        Self::Synastry { aspect_sig: aspect.sig() }
    }

    /// Plain-English label suitable for UI display.
    ///
    /// Examples:
    /// - `Natal { "moon_trine_venus" }`     → `"Moon trine Venus"`
    /// - `Transit { saturn, sun, square }`  → `"Saturn square Sun"`
    /// - `HouseTransit { jupiter, 7 }`      → `"Jupiter in house 7"`
    /// - `Placement { jupiter, 5, Some(9) }` → `"Jupiter in Virgo, House 9"`
    /// - `Placement { jupiter, 5, None }`    → `"Jupiter in Virgo"`
    pub fn plain_name(&self) -> String {
        match self {
            Self::Natal { aspect_sig }
            | Self::Synastry  { aspect_sig }
            | Self::SkyAspect { aspect_sig } => parse_aspect_sig(aspect_sig),
            Self::Transit { transiting, natal_body, kind } => {
                format!(
                    "{} {} {}",
                    cap(transiting.name()), kind.display_name(), cap(natal_body.name())
                )
            }
            Self::HouseTransit { transiting, house } => {
                format!("{} transiting {house} house", cap(transiting.name()))
            }
            Self::Placement { planet, sign, house } => match house {
                Some(h) => format!(
                    "{} in {}, House {h}", cap(planet.name()), cap(sign_name_lower(*sign)),
                ),
                None => format!("{} in {}", cap(planet.name()), cap(sign_name_lower(*sign))),
            },
            Self::PlacementAngle { angle, sign } => {
                format!("{} in {}", angle.display_name(), cap(sign_name_lower(*sign)))
            }
        }
    }
}

/// Best-effort reverse of `to_sig()`: parse a canonical key string back into
/// an `InterpKey`.  Returns `None` for `sky:` prefixed strings (global sky
/// aspects don't map to any existing variant — caller renders them in a
/// transit-detail-like view directly).
pub fn parse_interp_sig(sig: &str) -> Option<InterpKey> {
    let (kind, rest) = sig.split_once(':')?;
    use crate::aspects::AspectKind;
    match kind {
        "natal"        => Some(InterpKey::Natal     { aspect_sig: rest.to_string() }),
        "synastry"     => Some(InterpKey::Synastry  { aspect_sig: rest.to_string() }),
        "sky"          => Some(InterpKey::SkyAspect { aspect_sig: rest.to_string() }),
        "transit" => {
            // "transiting_kind_natal", e.g. "venus_trine_sun".  Find the
            // longest-matching aspect-kind name to split.
            const ALL: &[AspectKind] = &[
                AspectKind::SemiSextile,
                AspectKind::Conjunction, AspectKind::Sextile, AspectKind::Square,
                AspectKind::Trine, AspectKind::Quincunx, AspectKind::Opposition,
            ];
            for k in ALL {
                let needle = format!("_{}_", k.name());
                if let Some(pos) = rest.find(&needle) {
                    let transiting = crate::planet::Planet::from_name(&rest[..pos])?;
                    let natal_body = crate::planet::Planet::from_name(&rest[pos + needle.len()..])?;
                    return Some(InterpKey::Transit { transiting, natal_body, kind: *k });
                }
            }
            None
        }
        "house_transit" => {
            // "planet:house"
            let (p, h) = rest.split_once(':')?;
            let transiting = crate::planet::Planet::from_name(p)?;
            let house: u8  = h.parse().ok()?;
            Some(InterpKey::HouseTransit { transiting, house })
        }
        "placement" => {
            // "planet:sign" or "planet:sign:house"
            let mut parts = rest.splitn(3, ':');
            let planet = crate::planet::Planet::from_name(parts.next()?)?;
            let sign_name = parts.next()?;
            let sign = (0..12).find(|&i| sign_name_lower(i) == sign_name)?;
            let house = match parts.next() {
                Some(h) => Some(h.parse::<u8>().ok()?),
                None    => None,
            };
            Some(InterpKey::Placement { planet, sign, house })
        }
        // Placement angles omitted — feed cards don't surface them yet.
        _ => None,
    }
}

/// Human-readable kind badge ("Transit", "Synastry", …) straight from a
/// canonical key string's `kind:` prefix — same vocabulary `InterpKey::
/// kind()`'s variants map to, but usable where only the sig string is
/// available (e.g. the activity feed's `interp_key: String` fields).
pub fn kind_label_for_sig(sig: &str) -> &'static str {
    let kind = sig.split_once(':').map(|(k, _)| k).unwrap_or(sig);
    match kind {
        "natal"           => "Natal",
        "synastry"        => "Synastry",
        "transit"         => "Transit",
        "sky"             => "Sky",
        "house_transit"   => "House transit",
        "placement" | "placement_angle" => "Placement",
        _ => "Reading",
    }
}

/// Best-effort plain-English rendering of a canonical key string
/// (`"natal:venus_trine_jupiter"`, `"transit:mars_square_moon"`, `"sky:mars_conjunction_sun"`,
/// etc).  Used by the activity feed where we have the string but not the
/// parsed `InterpKey` (e.g. transit-ticker outputs, synthetic sky-aspect keys).
pub fn humanize_key(sig: &str) -> String {
    let (kind, rest) = match sig.split_once(':') {
        Some((k, r)) => (k, r),
        None         => ("", sig),
    };
    // Variant-specific phrasing for the keys that have one.
    match kind {
        "house_transit" => {
            // rest = "planet:house" — e.g. "mars:11" → "Mars transiting 11 house".
            if let Some((planet, house)) = rest.split_once(':') {
                return format!("{} transiting {house} house", cap(planet));
            }
        }
        "placement" if rest.contains(':') => {
            let mut parts = rest.splitn(3, ':');
            if let (Some(planet), Some(sign)) = (parts.next(), parts.next()) {
                return match parts.next() {
                    Some(house) => format!("{} in {}, House {house}", cap(planet), cap(sign)),
                    None        => format!("{} in {}", cap(planet), cap(sign)),
                };
            }
        }
        "placement_angle" if rest.contains(':') => {
            if let Some((angle, sign)) = rest.split_once(':') {
                let angle_disp = match angle { "asc" => "Ascendant", "mc" => "Midheaven", a => a };
                return format!("{angle_disp} in {}", cap(sign));
            }
        }
        _ => {}
    }
    // Aspect-shaped sigs (natal / synastry / transit / sky): "a_kind_b".
    if rest.contains('_') && !rest.contains(':') {
        return parse_aspect_sig(rest);
    }
    // Fallback: capitalise each underscore-separated word.
    sig.split([':', '_'])
        .filter(|s| !s.is_empty())
        .map(cap)
        .collect::<Vec<_>>()
        .join(" ")
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// "moon_trine_venus" → "Moon trine Venus", handling "semi_sextile" correctly.
fn parse_aspect_sig(sig: &str) -> String {
    use crate::aspects::AspectKind;
    const ALL: &[AspectKind] = &[
        AspectKind::SemiSextile,  // longest first so it matches before "sextile"
        AspectKind::Conjunction, AspectKind::Sextile, AspectKind::Square,
        AspectKind::Trine, AspectKind::Quincunx, AspectKind::Opposition,
    ];
    for kind in ALL {
        let needle = format!("_{}_", kind.name());
        if let Some(pos) = sig.find(&needle) {
            let a = cap(&sig[..pos]);
            let b = cap(&sig[pos + needle.len()..]);
            return format!("{} {} {}", a, kind.display_name(), b);
        }
    }
    // fallback: just capitalise each word
    sig.split('_').map(cap).collect::<Vec<_>>().join(" ")
}
