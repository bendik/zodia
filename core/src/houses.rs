//! House system computation.
//!
//! Requires geographic coordinates, derived from the birth Geohash via the
//! `geohash` crate.  Precision of the decoded coordinates scales with geohash
//! length — a 5-char hash gives ~±2.5 km, which is ±0.1° in ASC at mid-latitudes.
//!
//! Implemented systems:
//!   - Placidus   — semi-arc method; most widely used in Western astrology (default)
//!   - Whole Sign — ASC determines the 1st house sign; each sign = one house
//!   - Equal House — each cusp is exactly 30° from the ASC
//!
//! Stubs (fall back to Equal):
//!   - Koch — similar iterative approach, not yet implemented

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
    pub fn compute(birth: &BirthData, kind: HouseKind) -> Result<Self, HouseError> {
        if birth.geohash.len() < 3 {
            return Err(HouseError::GeohashTooShort(birth.geohash.len()));
        }
        let (lat, lon) = decode_geohash(&birth.geohash)?;
        let asc  = ascendant(birth.jdn, lat, lon);
        let mc   = midheaven(birth.jdn, lon);
        let ramc = (gmst(birth.jdn) + lon).rem_euclid(360.0);
        let eps  = obliquity(birth.jdn);

        let cusps = match kind {
            HouseKind::WholeSign => whole_sign_cusps(asc),
            HouseKind::Equal     => equal_house_cusps(asc),
            HouseKind::Placidus  => placidus_cusps(ramc, lat, eps, mc, asc),
            // Koch: fall back to Placidus (close enough; full Koch to be added later)
            HouseKind::Koch      => placidus_cusps(ramc, lat, eps, mc, asc),
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

    let y = lst_rad.cos();
    let x = -(eps.sin() * lat_rad.tan() + eps.cos() * lst_rad.sin());
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

/// Placidus house cusps — semi-arc method.
///
/// Houses 1, 4, 7, 10 are fixed at ASC, IC, DSC, MC.
/// Intermediate cusps (11, 12, 2, 3) are found by dividing each quadrant's
/// semi-arc into thirds, solved iteratively (~5–10 iterations converge to <1e-6°).
///
/// `ramc`: Right Ascension of MC (= Local Sidereal Time, degrees)
/// `lat`:  geographic latitude (degrees)
/// `eps`:  obliquity of the ecliptic (degrees)
/// `mc`:   MC ecliptic longitude (degrees)
/// `asc`:  Ascendant ecliptic longitude (degrees)
fn placidus_cusps(ramc: f64, lat: f64, eps: f64, mc: f64, asc: f64) -> [f64; 12] {
    let phi = lat.to_radians();
    let eps_r = eps.to_radians();

    let mut cusps = [0.0f64; 12];
    cusps[0]  = asc;                              // H1  = ASC
    cusps[3]  = (mc + 180.0).rem_euclid(360.0);  // H4  = IC
    cusps[6]  = (asc + 180.0).rem_euclid(360.0); // H7  = DSC
    cusps[9]  = mc;                               // H10 = MC

    // H11, H12: 1/3 and 2/3 of the diurnal semi-arc from MC toward ASC.
    for (i, frac) in [(10usize, 1.0_f64 / 3.0), (11, 2.0 / 3.0)] {
        let ra = placidus_iter(ramc, frac, phi, eps_r, true);
        cusps[i] = ra_to_ecl_lon(ra, eps_r);
    }

    // H2, H3: 1/3 and 2/3 of the nocturnal semi-arc from IC *toward ASC*.
    // This arc runs in the *decreasing* RA direction (IC_RA → ASC_RA),
    // so we use negative fractions to step backward from IC.
    let ic_ra = (ramc + 180.0).rem_euclid(360.0);
    for (i, frac) in [(1usize, -1.0_f64 / 3.0), (2, -2.0 / 3.0)] {
        let ra = placidus_iter(ic_ra, frac, phi, eps_r, false);
        cusps[i] = ra_to_ecl_lon(ra, eps_r);
    }

    // Opposite cusps.
    cusps[4] = (cusps[10] + 180.0).rem_euclid(360.0); // H5  = opposite H11
    cusps[5] = (cusps[11] + 180.0).rem_euclid(360.0); // H6  = opposite H12
    cusps[7] = (cusps[1]  + 180.0).rem_euclid(360.0); // H8  = opposite H2
    cusps[8] = (cusps[2]  + 180.0).rem_euclid(360.0); // H9  = opposite H3

    cusps
}

/// Iterate to find the RA of one Placidus intermediate cusp.
///
/// `base_ra`: starting RA of the quadrant (RAMC for H11/12, IC_RA for H2/3).
/// `frac`:    1/3 or 2/3.
/// `diurnal`: true → use diurnal semi-arc (DSA); false → nocturnal (NSA = π − DSA).
fn placidus_iter(base_ra: f64, frac: f64, phi: f64, eps: f64, diurnal: bool) -> f64 {
    // Seed the iteration with a 60° (1/3 of 180°) or 120° (2/3) offset.
    let mut ra = (base_ra + frac * 180.0).rem_euclid(360.0);

    for _ in 0..50 {
        // Ecliptic longitude corresponding to this RA (assuming β = 0).
        let ecl = ra_to_ecl_lon(ra, eps).to_radians();
        // Declination of that ecliptic point.
        let dec = (eps.sin() * ecl.sin()).asin();
        // Diurnal semi-arc: the angular distance from the eastern horizon to the MC.
        let cos_dsa = (-phi.tan() * dec.tan()).clamp(-1.0, 1.0);
        let dsa = cos_dsa.acos(); // radians, 0–π
        let semi_arc = if diurnal { dsa } else { std::f64::consts::PI - dsa };
        let new_ra = (base_ra.to_radians() + frac * semi_arc)
            .to_degrees()
            .rem_euclid(360.0);
        if (new_ra - ra).abs() < 1e-6 {
            return new_ra;
        }
        ra = new_ra;
    }
    ra
}

/// Convert equatorial RA (degrees) to ecliptic longitude (degrees), assuming β = 0.
///
/// Inverse of: tan(RA) = sin(λ)·cos(ε) / cos(λ)  →  λ = atan2(sin(RA), cos(RA)·cos(ε))
fn ra_to_ecl_lon(ra: f64, eps: f64) -> f64 {
    let ra_r = ra.to_radians();
    ra_r.sin()
        .atan2(ra_r.cos() * eps.cos())
        .to_degrees()
        .rem_euclid(360.0)
}
