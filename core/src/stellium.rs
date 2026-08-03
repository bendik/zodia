//! Stellium detection — 3+ planets clustered in the same sign or house, a
//! well-known astrological pattern (conventionally read as a concentration
//! of energy/emphasis) that Zodia never surfaced despite already deriving
//! each planet's sign and house for placements.

use std::collections::HashMap;

use crate::chart::Chart;
use crate::planet::{Planet, PlanetPositions};

/// Conventional minimum: fewer than 3 bodies sharing a sign/house isn't
/// called a stellium by any astrological reference this app follows.
const MIN_STELLIUM_SIZE: usize = 3;

/// Groups of 3+ planets sharing the same zodiac sign (0 = Aries … 11 =
/// Pisces), sorted by sign index. Angles (ASC/MC) are excluded — same
/// convention as `natal_balance`, which counts planets, not chart points.
pub fn stelliums_by_sign(positions: &PlanetPositions) -> Vec<(u8, Vec<Planet>)> {
    let mut by_sign: HashMap<u8, Vec<Planet>> = HashMap::new();
    for &planet in Planet::all() {
        let Some(lon) = positions.get(planet) else { continue };
        let sign = (lon.rem_euclid(360.0) / 30.0).floor() as u8 % 12;
        by_sign.entry(sign).or_default().push(planet);
    }
    let mut groups: Vec<(u8, Vec<Planet>)> = by_sign.into_iter()
        .filter(|(_, planets)| planets.len() >= MIN_STELLIUM_SIZE)
        .collect();
    groups.sort_by_key(|(sign, _)| *sign);
    groups
}

/// Groups of 3+ planets sharing the same house (1-12), sorted by house
/// number. Empty when the chart's houses are a stub (geohash too coarse for
/// a real house computation) — a house-based stellium is meaningless
/// without real house cusps.
pub fn stelliums_by_house(chart: &Chart) -> Vec<(u8, Vec<Planet>)> {
    let is_stub = chart.houses.cusps.iter().all(|&c| c == 0.0);
    if is_stub {
        return Vec::new();
    }
    let mut by_house: HashMap<u8, Vec<Planet>> = HashMap::new();
    for &planet in Planet::all() {
        let Some(lon) = chart.positions.get(planet) else { continue };
        let house = chart.houses.house_of(lon);
        by_house.entry(house).or_default().push(planet);
    }
    let mut groups: Vec<(u8, Vec<Planet>)> = by_house.into_iter()
        .filter(|(_, planets)| planets.len() >= MIN_STELLIUM_SIZE)
        .collect();
    groups.sort_by_key(|(house, _)| *house);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positions_from(pairs: &[(Planet, f64)]) -> PlanetPositions {
        PlanetPositions(pairs.iter().copied().collect())
    }

    #[test]
    fn three_planets_in_one_sign_is_a_stellium() {
        // All three in Taurus (30-60°), spread across the sign so this
        // isn't just "three planets at the same degree" but a real
        // same-sign cluster.
        let positions = positions_from(&[
            (Planet::Sun, 35.0), (Planet::Mercury, 42.0), (Planet::Venus, 58.0),
            (Planet::Mars, 200.0), // elsewhere, shouldn't join the group
        ]);
        let groups = stelliums_by_sign(&positions);
        assert_eq!(groups.len(), 1);
        let (sign, mut planets) = groups[0].clone();
        assert_eq!(sign, 1); // Taurus
        planets.sort_by_key(|p| format!("{p:?}"));
        assert_eq!(planets.len(), 3);
        assert!(planets.contains(&Planet::Sun));
        assert!(planets.contains(&Planet::Mercury));
        assert!(planets.contains(&Planet::Venus));
    }

    #[test]
    fn two_planets_in_one_sign_is_not_a_stellium() {
        let positions = positions_from(&[
            (Planet::Sun, 35.0), (Planet::Mercury, 42.0),
            (Planet::Mars, 200.0), (Planet::Venus, 210.0),
        ]);
        assert_eq!(stelliums_by_sign(&positions).len(), 0, "no group has 3+");
    }

    #[test]
    fn planets_spread_across_all_signs_yield_no_stellium() {
        // One planet per sign, 10 signs used out of 12 — every group has
        // exactly 1 member, nowhere near the 3-body threshold.
        let positions = positions_from(&[
            (Planet::Sun, 5.0), (Planet::Moon, 35.0), (Planet::Mercury, 65.0),
            (Planet::Venus, 95.0), (Planet::Mars, 125.0), (Planet::Jupiter, 155.0),
            (Planet::Saturn, 185.0), (Planet::Uranus, 215.0), (Planet::Neptune, 245.0),
            (Planet::Pluto, 275.0),
        ]);
        assert_eq!(stelliums_by_sign(&positions).len(), 0);
    }

    #[test]
    fn house_stellium_is_empty_for_a_stub_chart() {
        // A stub chart (geohash too coarse for real houses) has all-zero
        // cusps — house-based grouping would be meaningless noise there,
        // not a real astrological signal.
        let birth = crate::birth::birth_from_coords(2_451_545.0, 59.9, 10.7, 9);
        let mut chart = Chart::compute(birth).expect("chart computes");
        chart.houses = crate::houses::HouseSystem::stub();
        assert_eq!(stelliums_by_house(&chart).len(), 0);
    }
}
