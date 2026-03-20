use std::collections::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Planet {
    Sun,
    Moon,
    Mercury,
    Venus,
    Mars,
    Jupiter,
    Saturn,
    Uranus,
    Neptune,
    Pluto,
}

impl Planet {
    pub fn all() -> &'static [Planet] {
        &[
            Planet::Sun,
            Planet::Moon,
            Planet::Mercury,
            Planet::Venus,
            Planet::Mars,
            Planet::Jupiter,
            Planet::Saturn,
            Planet::Uranus,
            Planet::Neptune,
            Planet::Pluto,
        ]
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Planet::Sun     => "☉\u{FE0E}",
            Planet::Moon    => "☽\u{FE0E}",
            Planet::Mercury => "☿\u{FE0E}",
            Planet::Venus   => "♀\u{FE0E}",
            Planet::Mars    => "♂\u{FE0E}",
            Planet::Jupiter => "♃\u{FE0E}",
            Planet::Saturn  => "♄\u{FE0E}",
            Planet::Uranus  => "♅\u{FE0E}",
            Planet::Neptune => "♆\u{FE0E}",
            Planet::Pluto   => "♇\u{FE0E}",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Planet::Sun     => "sun",
            Planet::Moon    => "moon",
            Planet::Mercury => "mercury",
            Planet::Venus   => "venus",
            Planet::Mars    => "mars",
            Planet::Jupiter => "jupiter",
            Planet::Saturn  => "saturn",
            Planet::Uranus  => "uranus",
            Planet::Neptune => "neptune",
            Planet::Pluto   => "pluto",
        }
    }
}

/// Geocentric ecliptic longitudes (degrees, 0–360) for each body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanetPositions(pub HashMap<Planet, f64>);

impl PlanetPositions {
    pub fn get(&self, planet: Planet) -> Option<f64> {
        self.0.get(&planet).copied()
    }
}
