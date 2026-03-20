pub mod announce;
pub mod channel;
pub mod network;

pub use channel::{ChannelMsg, DirectChannel, InterpEntry};
pub use network::{NetworkConfig, NetworkError, ZodiaNetwork};

use serde::{Deserialize, Serialize};
use zodia_core::BirthData;

// ── wire types ────────────────────────────────────────────────────────────────

/// A Tier-0 announce blob broadcast to the gossip swarm.
///
/// Contains only the coarse birth fingerprint and an ed25519 pubkey.
/// No name, no face, no exact time.  Signed so every receiver can verify
/// that the blob was produced by whoever holds that keypair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier0Blob {
    /// 3-char geohash prefix (~600 km)
    pub geohash_prefix: String,
    /// Solar month (0–11)
    pub solar_month: u8,
    /// ed25519 public key (32 bytes) — this IS the peer's p2panda/iroh NodeId
    pub pubkey: [u8; 32],
    /// ed25519 signature over CBOR(geohash_prefix, solar_month, pubkey)
    pub sig: Vec<u8>,
}

/// A Tier-1 blob exchanged over the iroh QUIC connection after mutual consent.
///
/// Transport is already noise-encrypted (iroh TLS), so this is sent in the
/// clear at the application layer — confidentiality comes from the channel.
///
/// After exchange, both sides run a 3-way X3DH to derive the session key
/// that seeds the message ratchet for all subsequent tier messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier1Blob {
    /// Exact birth data (city-level geohash + exact JDN)
    pub birth: BirthData,
    /// Static X25519 prekey — rotated periodically, not per-session
    pub prekey: [u8; 32],
    /// Ephemeral X25519 key — generated fresh for this Tier-1 handshake
    pub ephemeral: [u8; 32],
}

/// Peer identity — the peer's ed25519 public key bytes.
///
/// In p2panda-net this equals the iroh `NodeId`; both are derived from
/// the same underlying ed25519 keypair so the bytes are interchangeable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    /// Convert to the p2panda-net `NodeId` type for `Endpoint::connect`.
    ///
    /// `NodeId = iroh::NodeId` — constructed from the same 32-byte key.
    pub fn to_node_id(&self) -> p2panda_net::NodeId {
        // iroh::NodeId / iroh::PublicKey wraps a 32-byte ed25519 key.
        // `from_bytes` is the canonical constructor in iroh 0.x.
        p2panda_net::NodeId::from_bytes(&self.0)
            .expect("PeerId was constructed from valid ed25519 key bytes")
    }
}

// ── domain events ─────────────────────────────────────────────────────────────

/// Typed events emitted by the network layer to the application.
#[derive(Debug)]
pub enum ZodiaNetEvent {
    /// A new peer appeared on the gossip swarm with a valid Tier-0 blob.
    PeerDiscovered { peer_id: PeerId, blob: Tier0Blob },
    /// A previously seen peer has gone offline (gossip departure).
    PeerLeft { peer_id: PeerId },
    /// The peer has sent their Tier-1 blob over the direct channel.
    Tier1Received { peer_id: PeerId, blob: Tier1Blob },
    /// The peer is requesting a Tier-1 connection.
    SessionRequested { peer_id: PeerId },
    /// The peer has accepted our Tier-1 connection request.
    SessionAccepted { peer_id: PeerId },
    /// An established peer is requesting a voice call.
    CallOffer { from: PeerId, session_id: [u8; 32] },
    /// The remote peer accepted our outgoing call offer.
    CallAccepted { from: PeerId, session_id: [u8; 32] },
    /// The remote peer rejected our outgoing call offer.
    CallRejected { from: PeerId },
    /// The remote peer ended an active call.
    CallHungUp { from: PeerId },
    /// An incoming Tier-1 QUIC connection was accepted from a remote peer.
    IncomingChannel { peer_id: PeerId, channel: DirectChannel },
}

