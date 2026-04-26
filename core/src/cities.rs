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

/// Returns `true` if city data was compiled in (i.e. `cities1000.txt` was
/// present at build time). When `false`, [`search_cities`] always returns
/// an empty Vec and the UI should fall back to direct coordinate input.
pub fn has_cities() -> bool {
    !CITIES.is_empty()
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
