pub mod announce;
pub mod channel;
pub mod network;

pub use channel::{ChannelMsg, DirectChannel, InterpEntry, PeerStatus, RelayPayload};
pub use network::{NetworkConfig, NetworkError, ZodiaNetwork};

use serde::{Deserialize, Serialize};
use zodia_core::BirthData;

// ── wire types ────────────────────────────────────────────────────────────────

/// Anonymous announce blob broadcast to the gossip swarm.
///
/// Contains only a coarse birth fingerprint and the ed25519 pubkey.
/// No name, no face, no exact time.  Signed so every receiver can verify
/// that the blob was produced by whoever holds that keypair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnounceBlob {
    /// 3-char geohash prefix (~600 km)
    pub geohash_prefix: String,
    /// Solar month (0–11)
    pub solar_month: u8,
    /// ed25519 public key (32 bytes) — this IS the peer's p2panda/iroh NodeId
    pub pubkey: [u8; 32],
    /// ed25519 signature over CBOR(geohash_prefix, solar_month, pubkey)
    pub sig: Vec<u8>,
}

/// Exact birth data + key material exchanged after mutual consent.
///
/// Sent over the iroh QUIC connection (transport-encrypted by QUIC TLS).
/// After exchange, both sides run a 3-way X3DH to derive the session key
/// that seeds the message ratchet for all subsequent exchanges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsentBlob {
    /// Exact birth data (city-level geohash + exact JDN)
    pub birth: BirthData,
    /// Static X25519 prekey — rotated periodically, not per-session
    pub prekey: [u8; 32],
    /// Ephemeral X25519 key — generated fresh for this exchange
    pub ephemeral: [u8; 32],
    /// Stable X25519 relay key — senders encrypt blind-relay payloads to this
    /// key so a forwarding peer cannot read the message content.
    #[serde(default)]
    pub relay_pk: [u8; 32],
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
    /// A new peer appeared on the gossip swarm with a valid announce blob.
    PeerDiscovered { peer_id: PeerId, blob: AnnounceBlob },
    /// A previously seen peer has gone offline (gossip departure).
    PeerLeft { peer_id: PeerId },
    /// The peer has sent their consent blob over the direct channel.
    ConsentReceived { peer_id: PeerId, blob: ConsentBlob },
    /// The peer is requesting a consent exchange.
    SessionRequested { peer_id: PeerId },
    /// The peer has accepted our consent exchange request.
    SessionAccepted { peer_id: PeerId },
    /// An established peer is requesting a voice call.
    CallOffer { from: PeerId, session_id: [u8; 32] },
    /// The remote peer accepted our outgoing call offer.
    CallAccepted { from: PeerId, session_id: [u8; 32] },
    /// The remote peer rejected our outgoing call offer.
    CallRejected { from: PeerId },
    /// The remote peer ended an active call.
    CallHungUp { from: PeerId },
    /// An incoming QUIC connection was accepted from a remote peer.
    IncomingChannel { peer_id: PeerId, channel: DirectChannel },
    /// A plain-text chat message received from a connected peer.
    ChatReceived { from: PeerId, text: String },
    /// The peer sent an explicit presence update (Active / Away).
    PeerStatusChanged { peer_id: PeerId, status: PeerStatus },
    /// The direct channel to a connected peer has closed (peer went offline).
    PeerChannelClosed { peer_id: PeerId },
    /// A relay message arrived — `dest` is the intended final recipient.
    ///
    /// If `dest` is us: decrypt `payload` with our relay key and handle as chat.
    /// If `dest` is a connected peer: forward the raw `ChannelMsg::RelayMsg` on.
    RelayReceived { via: PeerId, dest: PeerId, payload: Vec<u8> },
    /// A peer pushed one or more new interpretations to us over their live
    /// Tier-1 channel (i.e. they just submitted a new interpretation while
    /// both peers were connected).
    InterpReceived { from: PeerId, entries: Vec<InterpEntry> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use zodia_core::BirthData;

    fn sample_consent_blob() -> ConsentBlob {
        ConsentBlob {
            birth: BirthData::new(2451545.0, "u4pruydqqvj"),
            prekey:    [0x11u8; 32],
            ephemeral: [0x22u8; 32],
            relay_pk:  [0x33u8; 32],
        }
    }

    #[test]
    fn consent_blob_cbor_round_trip() {
        let blob = sample_consent_blob();
        let mut buf = Vec::new();
        ciborium::into_writer(&blob, &mut buf).unwrap();
        let decoded: ConsentBlob = ciborium::from_reader(buf.as_slice()).unwrap();
        assert_eq!(blob.birth.jdn, decoded.birth.jdn);
        assert_eq!(blob.birth.geohash, decoded.birth.geohash);
        assert_eq!(blob.prekey, decoded.prekey);
        assert_eq!(blob.ephemeral, decoded.ephemeral);
        assert_eq!(blob.relay_pk, decoded.relay_pk);
    }

    #[test]
    fn consent_blob_relay_pk_serde_default() {
        // ConsentBlob with relay_pk = all zeros should survive a CBOR round-trip
        // and the #[serde(default)] attribute means missing field → zero array.
        let blob = ConsentBlob {
            birth: BirthData::new(2451545.0, "u4pru"),
            prekey:    [0x11u8; 32],
            ephemeral: [0x22u8; 32],
            relay_pk:  [0u8; 32],
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&blob, &mut buf).unwrap();
        let decoded: ConsentBlob = ciborium::from_reader(buf.as_slice()).unwrap();
        assert_eq!(decoded.relay_pk, [0u8; 32]);
    }

    #[test]
    fn peer_id_to_node_id_round_trips() {
        use ed25519_dalek::SigningKey;
        // Use a fixed seed — no rand dependency needed.
        let key = SigningKey::from_bytes(&[0x55u8; 32]);
        let bytes: [u8; 32] = *key.verifying_key().as_bytes();
        let peer_id = PeerId(bytes);
        let node_id = peer_id.to_node_id();
        assert_eq!(node_id.as_bytes(), &bytes);
    }
}
