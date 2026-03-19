//! ZodiaNetwork — composes p2panda-net's modular building blocks into a
//! single handle the app layer holds for all network operations.
//!
//! Component roles:
//!   AddressBook  — knows which peers are on which topics, manages reconnects
//!   Endpoint     — QUIC transport (iroh), the actual I/O primitive
//!   MdnsDiscovery — local LAN peer discovery
//!   Discovery    — internet-wide peer discovery via p2panda DHT random walk
//!   Gossip       — topic-scoped ephemeral broadcast (Tier-0 announcements)
//!
//! The app receives all domain events via a `tokio::sync::mpsc::Receiver<ZodiaNetEvent>`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::channel::{ChannelMsg, DirectChannel};
use crate::{PeerId, Tier0Blob, ZodiaNetEvent};
use iroh::protocol::{AcceptError, ProtocolHandler};
use ed25519_dalek::SigningKey;
use futures_util::StreamExt;
use p2panda_core::PrivateKey as PandaKey;
use p2panda_net::gossip::{GossipHandle, GossipSubscription};
use p2panda_net::iroh_mdns::{MdnsDiscovery, MdnsDiscoveryMode};
use p2panda_net::{AddressBook, Discovery, Endpoint, Gossip};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};
use zodia_core::{BirthData, compute_positions, topic_key_global, topic_keys_for_chart};

/// Zodia's network identifier — all nodes sharing this ID form one logical
/// overlay; peers on different IDs are invisible to each other.
const NETWORK_ID: [u8; 32] = *b"zodia-network-2024\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

/// Protocol identifier for direct peer-to-peer Tier-1 consent exchanges.
pub const TIER1_PROTOCOL: &[u8] = b"zodia/tier1/1";

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("address book: {0}")]
    AddressBook(String),
    #[error("endpoint: {0}")]
    Endpoint(String),
    #[error("gossip: {0}")]
    Gossip(String),
    #[error("discovery: {0}")]
    Discovery(String),
    #[error("publish failed")]
    Publish,
}

/// Configuration for spawning a ZodiaNetwork.
pub struct NetworkConfig {
    /// The peer's ed25519 identity key.  The same bytes are used as the
    /// p2panda identity (operation signing) AND the iroh NodeId (transport).
    pub signing_key: SigningKey,
}

/// The live network handle.  Holds all p2panda-net components alive.
///
/// Cheap to clone — all inner components are reference-counted.
pub struct ZodiaNetwork {
    // Kept alive for the interpretation index sync (LogSync wiring — coming in zodia-sync)
    #[allow(dead_code)]
    gossip: Gossip,
    /// One handle per degree-bucket topic derived from the peer's natal chart.
    topic_handles: Vec<GossipHandle>,
    endpoint: Endpoint,
    /// Pre-constructed, signed announce blob for this peer.
    my_blob: Tier0Blob,
    event_tx: mpsc::Sender<ZodiaNetEvent>,
    // Keep discovery components alive (their drop = shutdown).
    _address_book: AddressBook,
    _mdns: MdnsDiscovery,
    _discovery: Discovery,
}

impl ZodiaNetwork {
    /// Spawn all network components and return the network handle plus the
    /// event channel receiver the app should poll.
    #[instrument(skip_all, fields(geohash = %birth.geohash))]
    pub async fn spawn(
        config: NetworkConfig,
        birth: &BirthData,
    ) -> Result<(Self, mpsc::Receiver<ZodiaNetEvent>), NetworkError> {
        // Derive the p2panda PrivateKey from the same scalar bytes.
        let panda_key = PandaKey::from_bytes(config.signing_key.as_bytes());

        let address_book = AddressBook::builder()
            .spawn()
            .await
            .map_err(|e| NetworkError::AddressBook(e.to_string()))?;
        debug!("address book ready");

        let endpoint = Endpoint::builder(address_book.clone())
            .network_id(NETWORK_ID)
            .private_key(panda_key)
            .spawn()
            .await
            .map_err(|e| NetworkError::Endpoint(e.to_string()))?;
        let node_id_hex = hex::encode_upper(&endpoint.node_id().as_bytes()[..4]);
        info!(node_id = %node_id_hex, "endpoint ready");

        let mdns = MdnsDiscovery::builder(address_book.clone(), endpoint.clone())
            .mode(MdnsDiscoveryMode::Active)
            .spawn()
            .await
            .map_err(|e| NetworkError::Discovery(e.to_string()))?;
        debug!("mDNS discovery active");

        let discovery = Discovery::builder(address_book.clone(), endpoint.clone())
            .spawn()
            .await
            .map_err(|e| NetworkError::Discovery(e.to_string()))?;
        debug!("DHT discovery active");

        let gossip = Gossip::builder(address_book.clone(), endpoint.clone())
            .spawn()
            .await
            .map_err(|e| NetworkError::Gossip(e.to_string()))?;
        debug!("gossip engine ready");

        // Always include the global topic so peers are discoverable even when
        // the network is small and no aspect-bucket overlap exists yet.
        // Aspect-derived topics layer on top for meaningful synastry matching
        // as the network grows.
        let mut topic_keys = vec![topic_key_global()];
        match compute_positions(birth.jdn) {
            Ok(positions) => {
                let aspect_keys = topic_keys_for_chart(&positions);
                info!(global = 1, aspect = aspect_keys.len(), "subscribing to topics");
                topic_keys.extend(aspect_keys);
            }
            Err(e) => {
                warn!("ephemeris error, skipping aspect topics: {e}");
            }
        };

        let mut topic_handles = Vec::with_capacity(topic_keys.len());
        for key in &topic_keys {
            let handle = gossip
                .stream(key.0)
                .await
                .map_err(|e| NetworkError::Gossip(e.to_string()))?;
            topic_handles.push(handle);
        }

        let my_blob = Tier0Blob::sign(birth, &config.signing_key);
        let (event_tx, event_rx) = mpsc::channel(256);

        // Register the Tier-1 ALPN so incoming connections are accepted.
        // Without this registration iroh refuses the TLS handshake with
        // "error 120: peer doesn't support any known protocol".
        endpoint
            .accept(TIER1_PROTOCOL, Tier1Handler { event_tx: event_tx.clone() })
            .await
            .map_err(|e| NetworkError::Endpoint(e.to_string()))?;
        debug!("tier-1 ALPN registered");

        // Dedup set shared across all topic listeners — a peer may be
        // discoverable via several shared topics and we only want one event.
        let seen_peers: Arc<Mutex<HashSet<[u8; 32]>>> = Arc::new(Mutex::new(HashSet::new()));
        for handle in &topic_handles {
            spawn_gossip_listener(handle.subscribe(), event_tx.clone(), Arc::clone(&seen_peers));
        }

        let net = ZodiaNetwork {
            gossip,
            topic_handles,
            endpoint,
            my_blob,
            event_tx,
            _address_book: address_book,
            _mdns: mdns,
            _discovery: discovery,
        };
        info!("ZodiaNetwork spawned");
        Ok((net, event_rx))
    }

    /// Broadcast our signed Tier-0 announce blob to both swarm topics.
    ///
    /// Call this once on startup and again whenever re-announcing (e.g. after
    /// a long offline period).
    #[instrument(skip(self))]
    pub async fn publish_announce(&self) -> Result<(), NetworkError> {
        let bytes = self.my_blob.to_cbor();
        for handle in &self.topic_handles {
            handle
                .publish(bytes.clone())
                .await
                .map_err(|_| NetworkError::Publish)?;
        }
        info!(topics = self.topic_handles.len(), "tier-0 announce published");
        Ok(())
    }

    /// Open a direct QUIC connection to `peer` for the Tier-1 handshake.
    ///
    /// Both sides then use `channel::tier1_exchange` to exchange `Tier1Blob`s
    /// and derive the shared session key via X3DH.
    #[instrument(skip(self), fields(peer = %hex::encode_upper(&peer.0[..4])))]
    pub async fn connect_peer(&self, peer: &PeerId) -> Result<crate::channel::DirectChannel, NetworkError> {
        debug!("opening tier-1 QUIC connection");
        let node_id = peer.to_node_id();
        let conn = self
            .endpoint
            .connect(node_id, TIER1_PROTOCOL)
            .await
            .map_err(|e| NetworkError::Endpoint(e.to_string()))?;
        info!("tier-1 channel open");
        Ok(crate::channel::DirectChannel::from_connection(conn))
    }

    /// Our own node ID (= public key bytes of the identity keypair).
    pub fn node_id(&self) -> PeerId {
        let bytes: [u8; 32] = *self.endpoint.node_id().as_bytes();
        PeerId(bytes)
    }

    /// A clone of the event sender — allows other subsystems (e.g. the AV
    /// layer) to inject events into the same stream.
    pub fn event_sender(&self) -> mpsc::Sender<ZodiaNetEvent> {
        self.event_tx.clone()
    }

    /// Register an established `DirectChannel` and start listening for incoming
    /// `ChannelMsg` messages from that peer, translating them into
    /// `ZodiaNetEvent`s on the shared event stream.
    ///
    /// Call this after a successful outgoing `connect_peer()` AND after
    /// accepting an incoming connection, so that call signaling is always
    /// handled in one place.
    pub fn accept_channel(&self, peer_id: PeerId, channel: DirectChannel) {
        spawn_channel_listener(peer_id, channel, self.event_tx.clone());
    }
}

// ── tier-1 protocol handler ───────────────────────────────────────────────────

/// Accepts incoming `TIER1_PROTOCOL` QUIC connections and emits an
/// `IncomingChannel` event so the app layer can register the channel.
#[derive(Debug, Clone)]
struct Tier1Handler {
    event_tx: mpsc::Sender<ZodiaNetEvent>,
}

impl ProtocolHandler for Tier1Handler {
    async fn accept(&self, conn: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let peer_id = PeerId(*conn.remote_id().as_bytes());
        let channel = DirectChannel::from_connection(conn);
        let _ = self.event_tx.send(ZodiaNetEvent::IncomingChannel { peer_id, channel }).await;
        Ok(())
    }
}

// ── channel listener ──────────────────────────────────────────────────────────

/// Spawn a background task that reads `ChannelMsg`s from `channel` and
/// forwards them as `ZodiaNetEvent`s.
pub(crate) fn spawn_channel_listener(
    peer_id: PeerId,
    channel: DirectChannel,
    tx: mpsc::Sender<ZodiaNetEvent>,
) {
    tokio::spawn(async move {
        loop {
            match channel.recv_msg().await {
                Ok(ChannelMsg::CallOffer { session_id }) => {
                    let _ = tx.send(ZodiaNetEvent::CallOffer { from: peer_id.clone(), session_id }).await;
                }
                Ok(ChannelMsg::CallAccept { session_id }) => {
                    let _ = tx.send(ZodiaNetEvent::CallAccepted { from: peer_id.clone(), session_id }).await;
                }
                Ok(ChannelMsg::CallReject { .. }) => {
                    let _ = tx.send(ZodiaNetEvent::CallRejected { from: peer_id.clone() }).await;
                }
                Ok(ChannelMsg::CallHangup { .. }) => {
                    let _ = tx.send(ZodiaNetEvent::CallHungUp { from: peer_id.clone() }).await;
                }
                Ok(_) => {} // Tier1Handshake already handled at connect time
                Err(e) => {
                    debug!(peer = %hex::encode_upper(&peer_id.0[..4]), err = %e, "channel closed");
                    break;
                }
            }
        }
    });
}

// ── gossip listener ───────────────────────────────────────────────────────────

fn spawn_gossip_listener(
    mut sub: GossipSubscription,
    tx: mpsc::Sender<ZodiaNetEvent>,
    seen: Arc<Mutex<HashSet<[u8; 32]>>>,
) {
    tokio::spawn(async move {
        while let Some(result) = sub.next().await {
            match result {
                Ok(bytes) => match Tier0Blob::from_cbor(&bytes) {
                    Ok(blob) if blob.verify().is_ok() => {
                        // Dedup: a peer may appear on several shared topics.
                        let is_new = seen.lock().map(|mut s| s.insert(blob.pubkey)).unwrap_or(true);
                        if !is_new {
                            continue;
                        }
                        let peer_id = PeerId(blob.pubkey);
                        debug!(
                            peer = %hex::encode_upper(&peer_id.0[..4]),
                            geohash = %blob.geohash_prefix,
                            solar_month = blob.solar_month,
                            "peer discovered via gossip"
                        );
                        let _ = tx.send(ZodiaNetEvent::PeerDiscovered { peer_id, blob }).await;
                    }
                    Ok(_) => {
                        warn!("gossip: received blob with invalid signature — discarding");
                    }
                    Err(e) => {
                        debug!(err = %e, "gossip: CBOR decode error — discarding");
                    }
                },
                Err(_lagged) => {
                    warn!("gossip: broadcast channel lagged — messages dropped");
                }
            }
        }
    });
}
