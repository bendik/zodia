//! House system computation.
//!
//! Requires geographic coordinates, derived from the birth Geohash via the
//! `geohash` crate.  Precision of the decoded coordinates scales with geohash
//! length — a 5-char hash gives ~±2.5 km, which is ±0.1° in ASC at mid-latitudes.
//!
//! Implemented systems:
//!   - Whole Sign — ASC determines the 1st house sign; each sign = one house
//!   - Equal House — each cusp is exactly 30° from the ASC
//!
//! Stubs (fall back to Equal):
//!   - Placidus and Koch require iterative latitude-dependent solving;
//!     they will be added in a dedicated follow-up.

use crate::birth::BirthData;
use crate::planet::{Planet, PlanetPositions};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HouseKind {
    Placidus,
    WholeSign,
    Koch,
    Equal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HouseSystem {
    pub kind: HouseKind,
    /// Cusp longitudes (ecliptic degrees, 0–360), index 0 = 1st house cusp.
    pub cusps: [f64; 12],
    pub ascendant: f64,
    pub midheaven: f64,
}

#[derive(Debug, Error)]
pub enum HouseError {
    #[error("invalid geohash '{0}'")]
    InvalidGeohash(String),
    #[error("geohash too short for house computation (need ≥3 chars, got {0})")]
    GeohashTooShort(usize),
}

impl HouseSystem {
    /// Compute house cusps for `birth` using the requested system.
    ///
    /// Falls back to Equal House for Placidus/Koch until those are implemented.
    pub fn compute(birth: &BirthData, kind: HouseKind) -> Result<Self, HouseError> {
        if birth.geohash.len() < 3 {
            return Err(HouseError::GeohashTooShort(birth.geohash.len()));
        }
        let (lat, lon) = decode_geohash(&birth.geohash)?;
        let asc = ascendant(birth.jdn, lat, lon);
        let mc  = midheaven(birth.jdn, lon);

        let cusps = match kind {
            HouseKind::WholeSign            => whole_sign_cusps(asc),
            HouseKind::Equal                => equal_house_cusps(asc),
            // Placidus and Koch: fall back to Equal for now
            HouseKind::Placidus | HouseKind::Koch => equal_house_cusps(asc),
        };

        Ok(Self { kind, cusps, ascendant: asc, midheaven: mc })
    }

    /// All-zero placeholder for when geographic data is unavailable.
    pub fn stub() -> Self {
        Self {
            kind: HouseKind::WholeSign,
            cusps: [0.0; 12],
            ascendant: 0.0,
            midheaven: 0.0,
        }
    }

    /// Which house (1–12) a given ecliptic longitude occupies.
    pub fn house_of(&self, longitude: f64) -> u8 {
        let lon = longitude.rem_euclid(360.0);
        for i in 0..12 {
            let start = self.cusps[i];
            let end   = self.cusps[(i + 1) % 12];
            let contains = if start < end {
                lon >= start && lon < end
            } else {
                // cusp range wraps through 0°
                lon >= start || lon < end
            };
            if contains { return (i + 1) as u8; }
        }
        1 // unreachable in practice
    }

    /// House number for each transiting planet, using this chart's cusps.
    pub fn house_positions(&self, sky: &PlanetPositions) -> Vec<(Planet, u8)> {
        Planet::all()
            .iter()
            .filter_map(|&p| sky.get(p).map(|lon| (p, self.house_of(lon))))
            .collect()
    }
}

// ── coordinate helpers ───────────────────────────────────────────────────────

fn decode_geohash(hash: &str) -> Result<(f64, f64), HouseError> {
    let (coord, _, _) = geohash::decode(hash)
        .map_err(|_| HouseError::InvalidGeohash(hash.to_string()))?;
    Ok((coord.y, coord.x)) // (latitude, longitude)
}

// ── astronomical helpers ─────────────────────────────────────────────────────

/// Obliquity of the ecliptic (degrees) — single-term approximation, ~0.01° error.
fn obliquity(jdn: f64) -> f64 {
    let t = (jdn - 2451545.0) / 36525.0;
    23.439_291_111 - 0.013_004_2 * t
}

/// Greenwich Mean Sidereal Time (degrees).
fn gmst(jdn: f64) -> f64 {
    let d = jdn - 2451545.0;
    (280.460_618_37 + 360.985_647_366_29 * d).rem_euclid(360.0)
}

/// Ascendant — the ecliptic degree rising on the eastern horizon.
///
/// Formula: ASC = atan2(-cos(LST), sin(ε)·tan(φ) + cos(ε)·sin(LST))
/// where LST = local sidereal time, ε = obliquity, φ = geographic latitude.
fn ascendant(jdn: f64, lat: f64, lon: f64) -> f64 {
    let lst = (gmst(jdn) + lon).rem_euclid(360.0);
    let lst_rad = lst.to_radians();
    let eps = obliquity(jdn).to_radians();
    let lat_rad = lat.to_radians();

    let y = -lst_rad.cos();
    let x = eps.sin() * lat_rad.tan() + eps.cos() * lst_rad.sin();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Midheaven (MC) — the ecliptic degree culminating on the meridian.
///
/// Formula: MC = atan2(sin(LST), cos(LST)·cos(ε))
fn midheaven(jdn: f64, lon: f64) -> f64 {
    let lst = (gmst(jdn) + lon).rem_euclid(360.0);
    let lst_rad = lst.to_radians();
    let eps = obliquity(jdn).to_radians();
    lst_rad.sin().atan2(lst_rad.cos() * eps.cos())
        .to_degrees()
        .rem_euclid(360.0)
}

// ── house systems ─────────────────────────────────────────────────────────────

/// Whole Sign: the sign containing the ASC is House 1; each sign = one house.
fn whole_sign_cusps(asc: f64) -> [f64; 12] {
    let asc_sign = (asc / 30.0).floor() as u32;
    let mut cusps = [0.0f64; 12];
    for i in 0..12u32 {
        cusps[i as usize] = f64::from((asc_sign + i) % 12 * 30);
    }
    cusps
}

/// Equal House: House 1 cusp = ASC, each subsequent cusp +30°.
fn equal_house_cusps(asc: f64) -> [f64; 12] {
    let mut cusps = [0.0f64; 12];
    for i in 0..12 {
        cusps[i] = (asc + i as f64 * 30.0).rem_euclid(360.0);
    }
    cusps
}
