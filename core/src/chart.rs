use crate::aspects::{Aspect, AspectSet, SynastryAspect, compute_aspects, compute_synastry};
use crate::birth::BirthData;
use crate::ephemeris::{EphemerisError, compute_positions};
use crate::houses::{HouseKind, HouseSystem};
use crate::interp::InterpKey;
use crate::planet::PlanetPositions;
use crate::transit::{TransitSet, build_transit_set};
use serde::{Deserialize, Serialize};

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
}
