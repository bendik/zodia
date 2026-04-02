//! Offline city-name → (lat, lon) lookup backed by a GeoNames cities1000 snapshot.
//!
//! The underlying data is a static array generated at build time from
//! `core/data/cities1000.txt` (downloaded once; gitignored).  The array is
//! sorted by lower-cased ASCII name so prefix queries use binary search.

include!(concat!(env!("OUT_DIR"), "/cities_data.rs"));

/// A matched city returned by [`search_cities`].
#[derive(Debug, Clone, Copy)]
pub struct CityHit {
    pub name: &'static str,
    pub country: &'static str,
    pub lat: f32,
    pub lon: f32,
}

/// Return up to `limit` cities whose ASCII name starts with `prefix`
/// (case-insensitive), in alphabetical order.
pub fn search_cities(prefix: &str, limit: usize) -> Vec<CityHit> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let lower = prefix.to_lowercase();
    let start = CITIES.partition_point(|(key, ..)| *key < lower.as_str());
    CITIES[start..]
        .iter()
        .take_while(|(key, ..)| key.starts_with(lower.as_str()))
        .take(limit)
        .map(|&(_, name, country, lat, lon)| CityHit { name, country, lat, lon })
        .collect()
}
