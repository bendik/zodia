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

/// Global discovery topic — every Zodia peer subscribes to this.
///
/// All Tier-0 announce blobs flow through one shared gossip channel so that
/// any peer can discover any other peer regardless of birth time or location.
/// Synastry filtering happens at the app layer after discovery, not here.
///
/// The Tier-0 blob itself reveals only geohash prefix + solar month + pubkey,
/// which is no more sensitive than being on the network at all.
pub fn topic_key_global() -> TopicKey {
    hash_topic("zodia:v1:global")
}

/// Kept for compatibility — both redirect to the global topic.
#[deprecated(note = "use topic_key_global(); per-birth topics were too restrictive")]
pub fn topic_key_broad(_birth: &BirthData) -> TopicKey { topic_key_global() }
#[deprecated(note = "use topic_key_global(); per-birth topics were too restrictive")]
pub fn topic_key_narrow(_birth: &BirthData) -> TopicKey { topic_key_global() }

fn hash_topic(input: &str) -> TopicKey {
    TopicKey(*blake3::hash(input.as_bytes()).as_bytes())
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
