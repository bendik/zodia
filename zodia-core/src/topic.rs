use crate::birth::BirthData;
use serde::{Deserialize, Serialize};

/// 32-byte Blake3 hash identifying a p2panda swarm topic.
/// Passed to `zodia-net` which converts it to the network layer's `TopicId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TopicKey(pub [u8; 32]);

impl TopicKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Broad topic key: ~600 km geohash prefix + solar month (0–11).
///
/// Peers born in broadly similar times and places cluster here.
/// Safe to derive from Tier-0 announce data.
pub fn topic_key_broad(birth: &BirthData) -> TopicKey {
    let prefix = truncated(&birth.geohash, 3);
    let month = solar_month(birth.jdn);
    hash_topic(&format!("zodia:broad:{}:{}", prefix, month))
}

/// Narrow topic key: ~5 km geohash prefix + exact solar degree (0–359).
///
/// A much smaller neighbourhood — only peers with nearly identical solar
/// placement in the same city. Used for the second swarm subscription.
pub fn topic_key_narrow(birth: &BirthData) -> TopicKey {
    let prefix = truncated(&birth.geohash, 5);
    let deg = solar_longitude(birth.jdn) as u32;
    hash_topic(&format!("zodia:narrow:{}:{}", prefix, deg))
}

fn hash_topic(input: &str) -> TopicKey {
    TopicKey(*blake3::hash(input.as_bytes()).as_bytes())
}

fn truncated(s: &str, n: usize) -> &str {
    &s[..n.min(s.len())]
}

// ── solar position ───────────────────────────────────────────────────────────

/// Solar month (0 = Aries, 11 = Pisces) derived from the Sun's ecliptic longitude.
pub fn solar_month(jdn: f64) -> u8 {
    (solar_longitude(jdn) / 30.0).floor() as u8
}

/// Sun's apparent geocentric ecliptic longitude (degrees, 0–360).
///
/// Low-precision formula from Meeus §25 — accurate to ~0.01° for 1950–2050.
pub fn solar_longitude(jdn: f64) -> f64 {
    let d = jdn - 2451545.0;                              // days since J2000.0
    let l = 280.460 + 0.985_647_4 * d;                    // mean longitude (°)
    let g = (357.528 + 0.985_600_3 * d).to_radians();     // mean anomaly (rad)
    let lambda = l + 1.915 * g.sin() + 0.020 * (2.0 * g).sin();
    lambda.rem_euclid(360.0)
}
