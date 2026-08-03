//! Elemental / modal balance — a classic natal chart analysis ("your chart
//! is Fire-dominant", "you have a lot of Cardinal energy") that Zodia never
//! computed despite already deriving each planet's sign for placements.
//!
//! Every sign has a fixed element and modality; this is pure classification
//! with no ephemeris dependency, computed straight from the sign index
//! `lon_to_sign_deg`/`placements.rs` already produce (0 = Aries … 11 = Pisces).

use crate::chart::Chart;
use crate::planet::Planet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element { Fire, Earth, Air, Water }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modality { Cardinal, Fixed, Mutable }

impl Element {
    pub fn name(&self) -> &'static str {
        match self {
            Element::Fire  => "Fire",
            Element::Earth => "Earth",
            Element::Air   => "Air",
            Element::Water => "Water",
        }
    }
}

impl Modality {
    pub fn name(&self) -> &'static str {
        match self {
            Modality::Cardinal => "Cardinal",
            Modality::Fixed    => "Fixed",
            Modality::Mutable  => "Mutable",
        }
    }
}

/// Element for a zodiac sign index (0 = Aries … 11 = Pisces). The classical
/// triplicities repeat every 4 signs in zodiac order: Fire, Earth, Air,
/// Water, Fire, Earth, ...
pub fn sign_element(sign: u8) -> Element {
    match sign % 4 {
        0 => Element::Fire,
        1 => Element::Earth,
        2 => Element::Air,
        _ => Element::Water,
    }
}

/// Modality (quadruplicity) for a zodiac sign index. Repeats every 3 signs:
/// Cardinal, Fixed, Mutable, Cardinal, ...
pub fn sign_modality(sign: u8) -> Modality {
    match sign % 3 {
        0 => Modality::Cardinal,
        1 => Modality::Fixed,
        _ => Modality::Mutable,
    }
}

/// Planet counts per element/modality across all 10 tracked bodies in a
/// chart — the standard "elemental balance" summary. Angles (ASC/MC) are
/// deliberately excluded: the classical balance count is over planets, not
/// chart points, and including angles would double-count signs relative to
/// every published reference on this analysis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Balance {
    pub fire:  u8, pub earth: u8, pub air: u8, pub water: u8,
    pub cardinal: u8, pub fixed: u8, pub mutable: u8,
}

pub fn natal_balance(chart: &Chart) -> Balance {
    let mut b = Balance::default();
    for &planet in Planet::all() {
        let Some(lon) = chart.positions.get(planet) else { continue };
        let sign = ((lon.rem_euclid(360.0)) / 30.0).floor() as u8 % 12;
        match sign_element(sign) {
            Element::Fire  => b.fire  += 1,
            Element::Earth => b.earth += 1,
            Element::Air   => b.air   += 1,
            Element::Water => b.water += 1,
        }
        match sign_modality(sign) {
            Modality::Cardinal => b.cardinal += 1,
            Modality::Fixed    => b.fixed    += 1,
            Modality::Mutable  => b.mutable  += 1,
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sign_element_mapping_matches_classical_triplicities() {
        // 0=Aries..11=Pisces, in zodiac order.
        let expected = [
            Element::Fire, Element::Earth, Element::Air, Element::Water, // Aries..Cancer
            Element::Fire, Element::Earth, Element::Air, Element::Water, // Leo..Scorpio
            Element::Fire, Element::Earth, Element::Air, Element::Water, // Sagittarius..Pisces
        ];
        for (sign, &want) in expected.iter().enumerate() {
            assert_eq!(sign_element(sign as u8), want, "sign {sign}");
        }
    }

    #[test]
    fn known_sign_modality_mapping_matches_classical_quadruplicities() {
        let expected = [
            Modality::Cardinal, Modality::Fixed, Modality::Mutable, // Aries, Taurus, Gemini
            Modality::Cardinal, Modality::Fixed, Modality::Mutable, // Cancer, Leo, Virgo
            Modality::Cardinal, Modality::Fixed, Modality::Mutable, // Libra, Scorpio, Sagittarius
            Modality::Cardinal, Modality::Fixed, Modality::Mutable, // Capricorn, Aquarius, Pisces
        ];
        for (sign, &want) in expected.iter().enumerate() {
            assert_eq!(sign_modality(sign as u8), want, "sign {sign}");
        }
    }

    #[test]
    fn cancer_is_cardinal_water_not_fire_start_confusion() {
        // Cancer (index 3) is a classic source of off-by-one bugs since it's
        // where the element cycle restarts (index 4 = Leo = Fire again) —
        // assert it directly rather than trust the cyclic pattern alone.
        assert_eq!(sign_element(3), Element::Water);
        assert_eq!(sign_modality(3), Modality::Cardinal);
    }

    #[test]
    fn natal_balance_counts_sum_to_all_tracked_planets() {
        let birth = crate::birth::birth_from_coords(2_451_545.0, 59.9, 10.7, 9);
        let chart = Chart::compute(birth).expect("chart computes");
        let b = natal_balance(&chart);
        let element_total = b.fire + b.earth + b.air + b.water;
        let modality_total = b.cardinal + b.fixed + b.mutable;
        assert_eq!(element_total, Planet::all().len() as u8);
        assert_eq!(modality_total, Planet::all().len() as u8);
    }
}
