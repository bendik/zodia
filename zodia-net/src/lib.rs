//! p2panda network layer for Zodia.
//!
//! Responsibilities:
//! - Join/leave swarm topics derived from the user's chart via `zodia_core::topic_key_*`
//! - Verify and parse incoming Tier-0 announce blobs (CBOR + ed25519 sig)
//! - Emit typed `ZodiaNetEvent`s for the app layer to consume
//!
//! The `p2panda-net` dependency and `Network` wiring will be added when the
//! p2panda crate versions are pinned.

use serde::{Deserialize, Serialize};
use zodia_core::{BirthData, TopicKey};

/// A Tier-0 announce blob broadcast to the swarm.
///
/// Contains only the coarse birth fingerprint and an ed25519 pubkey.
/// No name, no face, no exact time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier0Blob {
    /// 3-char geohash prefix (~600 km)
    pub geohash_prefix: String,
    /// Solar month (0–11)
    pub solar_month: u8,
    /// ed25519 public key (32 bytes)
    pub pubkey: [u8; 32],
    /// Signature over (geohash_prefix || solar_month || pubkey)
    pub sig: Vec<u8>,
}

/// A Tier-1 blob exchanged over the noise-encrypted channel after mutual consent.
///
/// Contains the exact birth fingerprint needed for precise synastry computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier1Blob {
    pub birth: BirthData,
    /// X25519 prekey for X3DH key agreement
    pub prekey: [u8; 32],
}

/// Opaque peer identifier — wraps the p2panda `PeerId` once that dep is wired.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub [u8; 32]);

/// Domain events emitted by the network layer to the app.
#[derive(Debug)]
pub enum ZodiaNetEvent {
    PeerDiscovered { peer_id: PeerId, blob: Tier0Blob },
    PeerLeft        { peer_id: PeerId },
    Tier1Received   { peer_id: PeerId, blob: Tier1Blob },
    SessionRequested { peer_id: PeerId },
    SessionAccepted  { peer_id: PeerId },
}

/// Topics the local node should subscribe to, derived from the user's chart.
pub struct SwarmTopics {
    pub broad: TopicKey,
    pub narrow: TopicKey,
}

impl SwarmTopics {
    pub fn from_birth(birth: &BirthData) -> Self {
        Self {
            broad:  zodia_core::topic_key_broad(birth),
            narrow: zodia_core::topic_key_narrow(birth),
        }
    }
}
