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
    /// Transiting planet occupying a natal house
    HouseTransit,
}

/// Fully qualified key for a community interpretation entry.
///
/// Canonical DB string examples:
/// - `"natal:jupiter_trine_venus"`
/// - `"synastry:jupiter_trine_venus"`
/// - `"transit:saturn_square_natal_venus"`
/// - `"house_transit:saturn:7"`
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
    HouseTransit {
        transiting: Planet,
        /// Natal house number (1–12)
        house: u8,
    },
}

impl InterpKey {
    /// Canonical string written to the `interp_key` column in SQLite.
    pub fn to_sig(&self) -> String {
        match self {
            Self::Natal       { aspect_sig }  => format!("natal:{aspect_sig}"),
            Self::Synastry    { aspect_sig }  => format!("synastry:{aspect_sig}"),
            Self::Transit     { transiting, natal_body, kind } =>
                format!("transit:{}_{}_{}", transiting.name(), kind.name(), natal_body.name()),
            Self::HouseTransit { transiting, house } =>
                format!("house_transit:{}:{house}", transiting.name()),
        }
    }

    pub fn kind(&self) -> InterpKind {
        match self {
            Self::Natal       { .. } => InterpKind::Natal,
            Self::Synastry    { .. } => InterpKind::Synastry,
            Self::Transit     { .. } => InterpKind::Transit,
            Self::HouseTransit { .. } => InterpKind::HouseTransit,
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
    /// - `Transit { saturn, sun, square }`  → `"Saturn square Sun (transit)"`
    /// - `HouseTransit { jupiter, 7 }`      → `"Jupiter in house 7"`
    pub fn plain_name(&self) -> String {
        match self {
            Self::Natal { aspect_sig }
            | Self::Synastry { aspect_sig } => parse_aspect_sig(aspect_sig),
            Self::Transit { transiting, natal_body, kind } => {
                format!(
                    "{} {} {}",
                    cap(transiting.name()), kind.display_name(), cap(natal_body.name())
                )
            }
            Self::HouseTransit { transiting, house } => {
                format!("{} in house {house}", cap(transiting.name()))
            }
        }
    }
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
