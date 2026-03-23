use serde::{Deserialize, Serialize};

/// Encode a lat/lon + JDN into a `BirthData` using the geohash crate.
///
/// `precision` sets the geohash length:  3 ≈ 600 km, 5 ≈ 5 km, 9 ≈ 2 m.
pub fn birth_from_coords(jdn: f64, lat: f64, lon: f64, precision: usize) -> BirthData {
    let coord = geohash::Coord { x: lon, y: lat };
    let gh = geohash::encode(coord, precision).unwrap_or_else(|_| "gcpvj".to_string());
    BirthData::new(jdn, gh)
}

/// Core identity of a birth moment — no calendar date, no timezone, no name.
///
/// `jdn` is the Julian Day Number (fractional), computed once from the user's
/// local datetime and stored in this canonical form.
///
/// `geohash` is a Geohash string at whatever precision the user consented to:
///   - 3 chars ≈ 600 km  (continent shard, safe for public broadcast)
///   - 5 chars ≈ 5 km    (city level, Tier-0 announce)
///   - 9 chars ≈ 2 m     (exact, Tier-1 exchange only)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BirthData {
    /// Julian Day Number (UT), e.g. 2451545.0 = 2000-01-01 12:00 UT
    pub jdn: f64,
    /// Geohash of birth location at negotiated precision
    pub geohash: String,
}

impl BirthData {
    pub fn new(jdn: f64, geohash: impl Into<String>) -> Self {
        Self { jdn, geohash: geohash.into() }
    }

    /// Truncate geohash to `precision` characters, returning a coarser BirthData.
    /// Useful when building announce blobs at lower consent tiers.
    pub fn at_precision(&self, precision: usize) -> Self {
        Self {
            jdn: self.jdn,
            geohash: self.geohash[..precision.min(self.geohash.len())].to_string(),
        }
    }
}
