use crate::aspects::{Aspect, AspectSet, SynastryAspect, compute_aspects, compute_synastry};
use crate::birth::BirthData;
use crate::ephemeris::{EphemerisError, compute_positions};
use crate::houses::{HouseKind, HouseSystem};
use crate::interp::InterpKey;
use crate::planet::{Planet, PlanetPositions};
use crate::transit::{TransitSet, build_transit_set};
use serde::{Deserialize, Serialize};

/// Sign indices (0 = Aries … 11 = Pisces) for the "Big Three" — Sun, Moon,
/// and Ascendant — the single most commonly asked-for at-a-glance summary
/// in modern pop astrology, despite every value it needs already being
/// computed for placements/houses separately with nowhere combining them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BigThree {
    pub sun_sign:       u8,
    pub moon_sign:      u8,
    pub ascendant_sign: u8,
}

fn sign_of(lon: f64) -> u8 {
    (lon.rem_euclid(360.0) / 30.0).floor() as u8 % 12
}

/// Whether `lon` falls in the last degree of its sign (29°00′–29°59′59″) —
/// the "critical" or "anaretic" degree, a well-defined, unambiguous
/// astrological concept (urgency/culmination themes) with no prior support
/// in this app despite needing nothing beyond the longitude itself.
pub fn is_critical_degree(lon: f64) -> bool {
    (lon.rem_euclid(360.0) % 30.0) >= 29.0
}

/// A fully computed natal chart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chart {
    pub birth: BirthData,
    pub positions: PlanetPositions,
    pub houses: HouseSystem,
}

impl Chart {
    /// Compute a chart using Placidus houses (default).
    /// Falls back to an all-zero house stub if the geohash is too coarse.
    pub fn compute(birth: BirthData) -> Result<Self, EphemerisError> {
        Self::compute_with(birth, HouseKind::Placidus)
    }

    /// Compute a chart with an explicit house system.
    pub fn compute_with(birth: BirthData, kind: HouseKind) -> Result<Self, EphemerisError> {
        let positions = compute_positions(birth.jdn)?;
        let houses = HouseSystem::compute(&birth, kind)
            .unwrap_or_else(|_| HouseSystem::stub());
        Ok(Self { birth, positions, houses })
    }

    // ── natal ────────────────────────────────────────────────────────────────

    /// All natal aspects for this chart.
    pub fn natal_aspects(&self) -> Vec<Aspect> {
        compute_aspects(&self.positions)
    }

    /// `InterpKey`s for every natal aspect — for bulk interpretation lookup.
    pub fn natal_interp_keys(&self) -> Vec<InterpKey> {
        self.natal_aspects().iter().map(InterpKey::from_natal).collect()
    }

    // ── synastry ─────────────────────────────────────────────────────────────

    /// Cross-chart aspects with another chart.
    pub fn synastry_with(&self, other: &Chart) -> Vec<SynastryAspect> {
        compute_synastry(&self.positions, &other.positions)
    }

    /// Full aspect set: natal + synastry with another chart.
    pub fn aspect_set_with(&self, other: &Chart) -> AspectSet {
        AspectSet {
            aspects: self.natal_aspects(),
            synastry: Some(self.synastry_with(other)),
        }
    }

    /// `InterpKey`s for every synastry aspect with `other`.
    pub fn synastry_interp_keys(&self, other: &Chart) -> Vec<InterpKey> {
        self.synastry_with(other).iter().map(InterpKey::from_synastry).collect()
    }

    // ── transits ─────────────────────────────────────────────────────────────

    /// Compute current transits to this natal chart at the given JDN.
    ///
    /// Returns both planet-to-planet transit aspects and house transits
    /// (which transiting planet is in which natal house).
    pub fn transits_at(&self, transit_jdn: f64) -> Result<TransitSet, EphemerisError> {
        let sky = compute_positions(transit_jdn)?;
        let house_transits = self.houses
            .house_positions(&sky)
            .into_iter()
            .map(|(planet, house)| crate::transit::HouseTransit { transiting: planet, house })
            .collect();
        Ok(build_transit_set(transit_jdn, sky, &self.positions, house_transits))
    }

    // ── summary ──────────────────────────────────────────────────────────────

    /// Sun sign, Moon sign, and Ascendant sign — `None` only if Sun or Moon
    /// is somehow missing from `positions` (shouldn't happen for a chart
    /// that computed successfully; Ascendant always has a value, real or stub).
    pub fn big_three(&self) -> Option<BigThree> {
        let sun  = self.positions.get(Planet::Sun)?;
        let moon = self.positions.get(Planet::Moon)?;
        Some(BigThree {
            sun_sign:       sign_of(sun),
            moon_sign:      sign_of(moon),
            ascendant_sign: sign_of(self.houses.ascendant),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_three_pulls_the_right_body_into_the_right_field() {
        // The real bug class this guards against: three near-identical
        // longitude lookups where copy-paste swaps which field gets which
        // body. Uses three longitudes in three different, easily
        // distinguished signs so a swap would be caught immediately.
        let birth = crate::birth::birth_from_coords(2_451_545.0, 59.9, 10.7, 9);
        let mut chart = Chart::compute(birth).expect("chart computes");
        chart.positions.0.insert(Planet::Sun, 10.0);   // Aries
        chart.positions.0.insert(Planet::Moon, 100.0); // Cancer
        chart.houses.ascendant = 190.0;                // Libra

        let bt = chart.big_three().expect("sun and moon are present");
        assert_eq!(bt.sun_sign, 0);
        assert_eq!(bt.moon_sign, 3);
        assert_eq!(bt.ascendant_sign, 6);
    }

    #[test]
    fn big_three_is_none_without_sun_or_moon() {
        let birth = crate::birth::birth_from_coords(2_451_545.0, 59.9, 10.7, 9);
        let mut chart = Chart::compute(birth).expect("chart computes");
        chart.positions.0.remove(&Planet::Moon);
        assert!(chart.big_three().is_none());
    }

    #[test]
    fn critical_degree_is_only_the_last_degree_of_a_sign() {
        assert!(!is_critical_degree(0.0));    // 0° Aries — start of sign
        assert!(!is_critical_degree(28.99));  // just short
        assert!(is_critical_degree(29.0));    // exactly the boundary
        assert!(is_critical_degree(29.99));   // last moment of the sign
    }

    #[test]
    fn critical_degree_applies_per_sign_not_just_near_0_aries() {
        // 30° = 0° Taurus (not critical); 59° = 29° Taurus (critical).
        // Guards against an implementation that only checks the raw
        // longitude's fractional part instead of the within-sign degree.
        assert!(!is_critical_degree(30.0));
        assert!(is_critical_degree(59.5));
    }
}
