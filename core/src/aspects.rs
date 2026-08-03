use crate::planet::{Planet, PlanetPositions};
use serde::{Deserialize, Serialize};

/// Classical and modern aspect types with default orbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AspectKind {
    Conjunction,  //   0°
    SemiSextile,  //  30°
    Sextile,      //  60°
    Square,       //  90°
    Trine,        // 120°
    Quincunx,     // 150°
    Opposition,   // 180°
}

impl AspectKind {
    pub fn angle(self) -> f64 {
        match self {
            Self::Conjunction => 0.0,
            Self::SemiSextile => 30.0,
            Self::Sextile     => 60.0,
            Self::Square      => 90.0,
            Self::Trine       => 120.0,
            Self::Quincunx    => 150.0,
            Self::Opposition  => 180.0,
        }
    }

    /// Default orb (degrees). Tighter for minor aspects.
    pub fn default_orb(self) -> f64 {
        match self {
            Self::Conjunction => 8.0,
            Self::Opposition  => 8.0,
            Self::Trine       => 8.0,
            Self::Square      => 7.0,
            Self::Sextile     => 6.0,
            Self::SemiSextile => 3.0,
            Self::Quincunx    => 3.0,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Conjunction => "☌\u{FE0E}",
            Self::SemiSextile => "⚺\u{FE0E}",
            Self::Sextile     => "⚹\u{FE0E}",
            Self::Square      => "□\u{FE0E}",
            Self::Trine       => "△\u{FE0E}",
            Self::Quincunx    => "⚻\u{FE0E}",
            Self::Opposition  => "☍\u{FE0E}",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Conjunction => "conjunction",
            Self::SemiSextile => "semi_sextile",
            Self::Sextile     => "sextile",
            Self::Square      => "square",
            Self::Trine       => "trine",
            Self::Quincunx    => "quincunx",
            Self::Opposition  => "opposition",
        }
    }

    /// Human-readable display name (hyphen instead of underscore for semi-sextile).
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Conjunction => "conjunction",
            Self::SemiSextile => "semi-sextile",
            Self::Sextile     => "sextile",
            Self::Square      => "square",
            Self::Trine       => "trine",
            Self::Quincunx    => "quincunx",
            Self::Opposition  => "opposition",
        }
    }

    /// Every aspect kind this app recognizes, in the same canonical order
    /// as detection — for UIs that need to enumerate them (e.g. a glyph
    /// legend), rather than duplicating the variant list.
    pub fn all() -> &'static [AspectKind] {
        ASPECTS
    }
}

const ASPECTS: &[AspectKind] = &[
    AspectKind::Conjunction,
    AspectKind::SemiSextile,
    AspectKind::Sextile,
    AspectKind::Square,
    AspectKind::Trine,
    AspectKind::Quincunx,
    AspectKind::Opposition,
];

/// Canonical string key for an aspect, e.g. `"venus_trine_jupiter"`.
/// Used as the primary key in the interpretation index.
pub type AspectSig = String;

/// A natal aspect between two bodies in the same chart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aspect {
    pub body_a: Planet,
    pub body_b: Planet,
    pub kind: AspectKind,
    /// Angular deviation from exact (degrees). Positive = approaching exact.
    pub orb: f64,
}

impl Aspect {
    /// Stable string key, e.g. `"venus_trine_jupiter"`.
    /// Bodies are sorted so the sig is the same regardless of argument order.
    pub fn sig(&self) -> AspectSig {
        let (a, b) = sorted_names(self.body_a, self.body_b);
        format!("{}_{}_{}", a, self.kind.name(), b)
    }
}

/// A synastry aspect between a body in chart A and a body in chart B.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynastryAspect {
    /// Body from chart A
    pub body_a: Planet,
    /// Body from chart B
    pub body_b: Planet,
    pub kind: AspectKind,
    pub orb: f64,
}

impl SynastryAspect {
    pub fn sig(&self) -> AspectSig {
        let (a, b) = sorted_names(self.body_a, self.body_b);
        format!("{}_{}_{}", a, self.kind.name(), b)
    }
}

/// Combined natal + synastry aspects for a peer encounter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AspectSet {
    pub aspects: Vec<Aspect>,
    pub synastry: Option<Vec<SynastryAspect>>,
}

// ── computation ─────────────────────────────────────────────────────────────

/// Unsigned angular separation between two ecliptic longitudes (0–180°).
pub fn angular_separation(a: f64, b: f64) -> f64 {
    let d = (b - a).rem_euclid(360.0);
    if d > 180.0 { 360.0 - d } else { d }
}

/// Aspect detection against the standard orb table.
pub fn detect_aspect(sep: f64) -> Option<(AspectKind, f64)> {
    ASPECTS.iter().find_map(|&kind| {
        let orb = sep - kind.angle();
        (orb.abs() <= kind.default_orb()).then_some((kind, orb))
    })
}

/// Compute all natal aspects within a single set of positions.
pub fn compute_aspects(positions: &PlanetPositions) -> Vec<Aspect> {
    let planets = Planet::all();
    let mut aspects = Vec::new();
    for (i, &a) in planets.iter().enumerate() {
        for &b in &planets[i + 1..] {
            let Some(lon_a) = positions.get(a) else { continue };
            let Some(lon_b) = positions.get(b) else { continue };
            if let Some((kind, orb)) = detect_aspect(angular_separation(lon_a, lon_b)) {
                aspects.push(Aspect { body_a: a, body_b: b, kind, orb });
            }
        }
    }
    aspects
}

/// Compute cross-chart synastry aspects between two sets of positions.
pub fn compute_synastry(a: &PlanetPositions, b: &PlanetPositions) -> Vec<SynastryAspect> {
    let mut aspects = Vec::new();
    for &pa in Planet::all() {
        for &pb in Planet::all() {
            let Some(lon_a) = a.get(pa) else { continue };
            let Some(lon_b) = b.get(pb) else { continue };
            if let Some((kind, orb)) = detect_aspect(angular_separation(lon_a, lon_b)) {
                aspects.push(SynastryAspect { body_a: pa, body_b: pb, kind, orb });
            }
        }
    }
    aspects
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Sort two planet names lexicographically so aspect sigs are canonical.
fn sorted_names(a: Planet, b: Planet) -> (&'static str, &'static str) {
    let (na, nb) = (a.name(), b.name());
    if na <= nb { (na, nb) } else { (nb, na) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_aspect_kind_exactly_once_with_a_distinct_angle() {
        let all = AspectKind::all();
        assert_eq!(all.len(), 7, "expected all 7 classical/modern aspect kinds");
        let mut angles: Vec<f64> = all.iter().map(|k| k.angle()).collect();
        angles.sort_by(|a, b| a.partial_cmp(b).unwrap());
        angles.dedup();
        assert_eq!(angles.len(), 7, "two aspect kinds share an angle — a real detection bug");
    }
}
