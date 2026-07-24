//! Gossip topic derivation.
//!
//! # Degree-bucket topics
//!
//! Topics are derived from where a peer's personal planets sit in the ecliptic,
//! plus the degree positions that would form major aspects *to* those planets.
//! Two peers share a topic whenever any of their personal planets fall within
//! ~5° of forming a major aspect with the other's — i.e. real potential synastry.
//!
//! Cluster size stays bounded regardless of network growth: each 5° bucket
//! covers ~1.4% of the zodiac, so at 10 000 users a typical topic has ~14
//! members, at 1 000 000 users ~1 400 — far better than a single global channel.
//!
//! # Privacy
//!
//! Topics are hashed Blake3 digests; the raw degree is not recoverable without
//! already knowing the range to test.  The Tier-0 blob still reveals only
//! geohash prefix + solar month — exact planetary positions are only shared
//! after Tier-1 consent.

use std::collections::HashSet;

use crate::planet::{Planet, PlanetPositions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TopicKey(pub [u8; 32]);

impl TopicKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

// ── constants ─────────────────────────────────────────────────────────────────

/// Bucket width in ecliptic degrees.  Smaller = more specific topics but fewer
/// shared channels.  5° gives comfortable orbs and ~72 buckets per circle.
const BUCKET_DEG: f64 = 5.0;

/// Personal planets — fast movers that vary significantly between birth times.
/// Outer planets (Jupiter–Pluto) move so slowly that everyone born in the same
/// few years shares the same position; they don't usefully discriminate topics.
const PERSONAL_PLANETS: &[Planet] = &[
    Planet::Sun,
    Planet::Moon,
    Planet::Mercury,
    Planet::Venus,
    Planet::Mars,
];

/// Aspect offsets (°) applied to each planet position.
///
/// For each personal planet P at longitude L we subscribe to topics at
/// `(L + offset) mod 360` for every offset in this list.  This ensures that
/// a peer whose planet sits *at* that degree will also subscribe to the same
/// topic — creating the synastry "handshake".
const ASPECT_OFFSETS: &[f64] = &[
    0.0,   // conjunction (own position)
    180.0, // opposition
    90.0,  // square
    270.0, // square (other direction)
    120.0, // trine
    240.0, // trine (other direction)
];

// ── public API ────────────────────────────────────────────────────────────────

/// Derive all gossip topics for a peer given their natal planetary positions.
///
/// Returns at most `PERSONAL_PLANETS.len() × ASPECT_OFFSETS.len()` unique
/// topics (some will collide when planets are near each other).  In practice
/// ~20–28 topics per peer.
pub fn topic_keys_for_chart(positions: &PlanetPositions) -> Vec<TopicKey> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();

    for &planet in PERSONAL_PLANETS {
        if let Some(&lon) = positions.0.get(&planet) {
            for &offset in ASPECT_OFFSETS {
                let bucket = degree_bucket(lon + offset);
                if seen.insert(bucket) {
                    keys.push(hash_topic(&format!("zodia:v1:degree:{}", bucket)));
                }
            }
        }
    }

    keys
}

/// Fallback single topic used when chart positions are unavailable.
pub fn topic_key_global() -> TopicKey {
    hash_topic("zodia:v1:global")
}

/// Per-`interp_key` sync topic (Phase C-2 — granular subscription).  Scopes
/// LogSync replication to exactly the key a subscriber cares about, paired
/// with a `log_id` derived the same way (see `zodia_ops::log_id_for_key`)
/// so the log synced over this topic actually contains only this key's ops.
pub fn topic_key_for_interp(interp_key: &str) -> TopicKey {
    hash_topic(&format!("zodia:v1:interp:{interp_key}"))
}

// ── solar position helpers (kept for Tier-0 blob metadata) ───────────────────


impl serde::Serialize for TopicKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}
impl<'de> serde::Deserialize<'de> for TopicKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bytes = <[u8; 32] as serde::Deserialize>::deserialize(d)?;
        Ok(TopicKey(bytes))
    }
}

/// Solar month (0 = Aries … 11 = Pisces).  Used in Tier-0 blob metadata only.
pub fn solar_month(jdn: f64) -> u8 {
    (solar_longitude(jdn) / 30.0).floor() as u8
}

/// Sun's apparent geocentric ecliptic longitude (°, 0–360).
/// Low-precision Meeus §25 — accurate to ~0.01° for 1950–2050.
pub fn solar_longitude(jdn: f64) -> f64 {
    let d = jdn - 2451545.0;
    let l = 280.460 + 0.985_647_4 * d;
    let g = (357.528 + 0.985_600_3 * d).to_radians();
    let lambda = l + 1.915 * g.sin() + 0.020 * (2.0 * g).sin();
    lambda.rem_euclid(360.0)
}

// ── internal helpers ──────────────────────────────────────────────────────────

/// Map an ecliptic longitude to a discrete bucket index.
fn degree_bucket(lon: f64) -> u32 {
    (lon.rem_euclid(360.0) / BUCKET_DEG).floor() as u32
}

fn hash_topic(input: &str) -> TopicKey {
    TopicKey(*blake3::hash(input.as_bytes()).as_bytes())
}
