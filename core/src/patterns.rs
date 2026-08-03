//! Chart-wide aspect pattern detection — Grand Trine and T-Square, the two
//! most commonly discussed geometric aspect configurations in astrology.
//! `compute_aspects` already finds every individual aspect within orb; this
//! module looks for specific *combinations* of three that form a named
//! shape, which nothing in the app previously checked for at all.

use std::collections::HashMap;

use crate::aspects::{Aspect, AspectKind};
use crate::planet::Planet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartPattern {
    /// Three planets, each trine the other two — a closed triangle of ease.
    GrandTrine(Planet, Planet, Planet),
    /// Two planets in opposition, both square a third (the "apex").
    TSquare { apex: Planet, opposition: (Planet, Planet) },
}

fn pair_key(a: Planet, b: Planet) -> (Planet, Planet) {
    // Order-independent: aspects are symmetric (A trine B == B trine A).
    if a <= b { (a, b) } else { (b, a) }
}

/// Every Grand Trine / T-Square present among `aspects`. Only considers
/// planets that actually have at least one aspect in the input — a triple
/// with no aspect at all between two of its members can't form either
/// pattern, so it's skipped rather than treated as "unknown, assume no".
pub fn detect_patterns(aspects: &[Aspect]) -> Vec<ChartPattern> {
    let mut lookup: HashMap<(Planet, Planet), AspectKind> = HashMap::new();
    let mut seen_planets: Vec<Planet> = Vec::new();
    for a in aspects {
        lookup.insert(pair_key(a.body_a, a.body_b), a.kind);
        for p in [a.body_a, a.body_b] {
            if !seen_planets.contains(&p) { seen_planets.push(p); }
        }
    }

    let mut patterns = Vec::new();
    let n = seen_planets.len();
    for i in 0..n {
        for j in (i + 1)..n {
            for k in (j + 1)..n {
                let (p1, p2, p3) = (seen_planets[i], seen_planets[j], seen_planets[k]);
                let ab = lookup.get(&pair_key(p1, p2)).copied();
                let bc = lookup.get(&pair_key(p2, p3)).copied();
                let ac = lookup.get(&pair_key(p1, p3)).copied();

                if let (Some(AspectKind::Trine), Some(AspectKind::Trine), Some(AspectKind::Trine)) = (ab, bc, ac) {
                    patterns.push(ChartPattern::GrandTrine(p1, p2, p3));
                    continue;
                }

                // T-Square: whichever pair is the Opposition, the third
                // planet (the apex) must Square both of them.
                for (opp_pair, apex, sq1, sq2) in [
                    ((p1, p2), p3, ac, bc),
                    ((p1, p3), p2, ab, bc),
                    ((p2, p3), p1, ab, ac),
                ] {
                    let is_opposition = lookup.get(&pair_key(opp_pair.0, opp_pair.1)) == Some(&AspectKind::Opposition);
                    if is_opposition
                        && sq1 == Some(AspectKind::Square)
                        && sq2 == Some(AspectKind::Square)
                    {
                        patterns.push(ChartPattern::TSquare { apex, opposition: opp_pair });
                    }
                }
            }
        }
    }
    patterns
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aspect(a: Planet, b: Planet, kind: AspectKind) -> Aspect {
        Aspect { body_a: a, body_b: b, kind, orb: 1.0 }
    }

    #[test]
    fn three_mutual_trines_is_a_grand_trine() {
        let aspects = vec![
            aspect(Planet::Sun, Planet::Moon, AspectKind::Trine),
            aspect(Planet::Moon, Planet::Jupiter, AspectKind::Trine),
            aspect(Planet::Sun, Planet::Jupiter, AspectKind::Trine),
        ];
        let patterns = detect_patterns(&aspects);
        assert_eq!(patterns.len(), 1);
        assert!(matches!(patterns[0], ChartPattern::GrandTrine(..)));
    }

    #[test]
    fn opposition_plus_two_squares_is_a_t_square_with_the_right_apex() {
        // Sun opposite Moon; Mars squares both — Mars is the apex.
        let aspects = vec![
            aspect(Planet::Sun, Planet::Moon, AspectKind::Opposition),
            aspect(Planet::Mars, Planet::Sun, AspectKind::Square),
            aspect(Planet::Mars, Planet::Moon, AspectKind::Square),
        ];
        let patterns = detect_patterns(&aspects);
        assert_eq!(patterns.len(), 1);
        match patterns[0] {
            ChartPattern::TSquare { apex, opposition } => {
                assert_eq!(apex, Planet::Mars);
                assert!(opposition == (Planet::Sun, Planet::Moon) || opposition == (Planet::Moon, Planet::Sun));
            }
            other => panic!("expected TSquare, got {other:?}"),
        }
    }

    #[test]
    fn two_trines_without_the_third_leg_is_not_a_grand_trine() {
        // Sun-Moon and Moon-Jupiter are trine, but Sun-Jupiter was never
        // computed as an aspect at all (outside orb) — two legs isn't a
        // triangle.
        let aspects = vec![
            aspect(Planet::Sun, Planet::Moon, AspectKind::Trine),
            aspect(Planet::Moon, Planet::Jupiter, AspectKind::Trine),
        ];
        assert_eq!(detect_patterns(&aspects).len(), 0);
    }

    #[test]
    fn an_opposition_with_only_one_square_is_not_a_t_square() {
        let aspects = vec![
            aspect(Planet::Sun, Planet::Moon, AspectKind::Opposition),
            aspect(Planet::Mars, Planet::Sun, AspectKind::Square),
            // Mars-Moon deliberately not a square — say it's a sextile.
            aspect(Planet::Mars, Planet::Moon, AspectKind::Sextile),
        ];
        assert_eq!(detect_patterns(&aspects).len(), 0);
    }

    #[test]
    fn unrelated_aspects_yield_no_patterns() {
        let aspects = vec![
            aspect(Planet::Sun, Planet::Moon, AspectKind::Sextile),
            aspect(Planet::Venus, Planet::Mars, AspectKind::Conjunction),
        ];
        assert_eq!(detect_patterns(&aspects).len(), 0);
    }
}
