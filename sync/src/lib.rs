//! Offline-first interpretation sync for the Zodia community index.
//!
//! # Design
//!
//! Each user maintains an append-only p2panda log (log id `0`) of the
//! interpretations they have authored.  When two peers share the same sync
//! topic they perform a **set-reconciliation catch-up** (exchanging log
//! heights) and then enter **live mode** where newly published operations
//! are gossip-broadcast immediately.
//!
//! `ZodiaSyncNode` wraps `p2panda-net`'s `LogSync` and translates between the
//! p2panda operation layer and `ZodiaStore`'s application-level records.
//!
//! # Limitations
//!
//! * Only interpretations published *after* `ZodiaSyncNode::spawn` is called
//!   are entered into the p2panda log; older `ZodiaStore` community entries
//!   are displayed locally and shared via the existing Tier-1 direct channel
//!   but are not (yet) replicated through LogSync.
//! * The p2panda operation store is in-memory only; on restart the node
//!   re-syncs its log state from online peers.  Persistent storage can be
//!   added later by switching to `p2panda_store::SqliteStore`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use p2panda_core::{Body, Header, Operation, PrivateKey as PandaKey, PublicKey as PandaPublicKey};
use p2panda_net::sync::{LogSync, SyncHandle};
use p2panda_net::{Endpoint, Gossip, TopicId};
use p2panda_store::{LogStore, MemoryStore, OperationStore};
use p2panda_sync::protocols::{Logs, TopicLogSyncEvent};
use p2panda_sync::traits::TopicMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use zodia_store::{StoreError, ZodiaStore};

// ── log id ────────────────────────────────────────────────────────────────────

/// Each author has exactly one log containing all their interpretations.
const INTERP_LOG_ID: u64 = 0;

// ── payload ───────────────────────────────────────────────────────────────────

/// CBOR-encoded body of a p2panda interpretation operation.
///
/// The `author_sig` is the Zodia-level ed25519 signature over
/// `BLAKE3(interp_key || body)` — the same payload verified by
/// `ZodiaStore::insert_received`.  Because the p2panda header already carries
/// an ed25519 signature from the same key pair, this is redundant but lets us
/// feed received operations directly into `ZodiaStore::insert_received`
/// without modifying its verification contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterpPayload {
    pub interp_key: String,
    pub body: String,
    /// ed25519 signature, 64 bytes.
    pub author_sig: Vec<u8>,
}

impl InterpPayload {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(self, &mut buf).expect("ciborium encode infallible");
        buf
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        ciborium::from_reader(bytes).ok()
    }
}

// ── topic map ─────────────────────────────────────────────────────────────────

/// Maps a `TopicId` to the set of p2panda logs we know about.
///
/// For Zodia there is a single sync topic (the global Zodia topic).
/// Every peer that has ever published an interpretation is an "author" with
/// exactly one log (`INTERP_LOG_ID`).  The map is updated whenever we receive
/// a new operation from a previously unseen author so that future sync
/// sessions can propagate that author's history to other peers.
#[derive(Clone)]
pub struct InterpTopicMap {
    /// `HashMap<author_public_key, vec![INTERP_LOG_ID]>`
    logs: Arc<RwLock<HashMap<PandaPublicKey, Vec<u64>>>>,
}

impl InterpTopicMap {
    fn new() -> Self {
        Self {
            logs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register `author` as a known log holder (idempotent).
    pub async fn add_author(&self, author: PandaPublicKey) {
        let mut map = self.logs.write().await;
        map.entry(author).or_insert_with(|| vec![INTERP_LOG_ID]);
    }
}

impl TopicMap<TopicId, Logs<u64>> for InterpTopicMap {
    type Error = Infallible;

    async fn get(&self, _topic: &TopicId) -> Result<Logs<u64>, Infallible> {
        let map = self.logs.read().await;
        Ok(map.clone())
    }
}

// ── store alias ───────────────────────────────────────────────────────────────

type SyncStore = MemoryStore<u64, ()>;

// ── handle alias ─────────────────────────────────────────────────────────────

type InterpSyncHandle = SyncHandle<Operation<()>, TopicLogSyncEvent<()>>;

// ── sync node ─────────────────────────────────────────────────────────────────

/// Error type for sync operations.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("p2panda sync: {0}")]
    Sync(String),
    #[error("payload encode/decode failed")]
    Payload,
}

/// A decoded interpretation that arrived via LogSync from a remote peer.
#[derive(Debug, Clone)]
pub struct ReceivedInterp {
    pub interp_key: String,
    pub body: String,
    pub author_pk: [u8; 32],
    pub author_sig: [u8; 64],
}

/// The live sync handle.
///
/// Keeps the p2panda LogSync machinery alive and mediates between the
/// application and the sync layer.
pub struct ZodiaSyncNode {
    /// p2panda private key — same bytes as the Zodia identity `SigningKey`.
    panda_key: PandaKey,
    /// In-memory p2panda operation store.
    sync_store: SyncStore,
    /// LogSync handle for our single sync topic.
    handle: InterpSyncHandle,
    /// Interpretations received from remote peers, ready for the app to consume.
    pub received: mpsc::Receiver<ReceivedInterp>,
}

impl ZodiaSyncNode {
    /// Spawn the sync node.
    ///
    /// * `panda_key`    — the local identity private key
    /// * `endpoint`     — clone of `ZodiaNetwork`'s iroh endpoint
    /// * `gossip`       — clone of `ZodiaNetwork`'s gossip engine
    /// * `zodia_store`  — shared reference to the local `ZodiaStore` for
    ///                    persisting received interpretations
    /// * `sync_topic`   — 32-byte topic id (use `topic_key_global().0`)
    pub async fn spawn(
        panda_key: PandaKey,
        endpoint: Endpoint,
        gossip: Gossip,
        zodia_store: Arc<Mutex<ZodiaStore>>,
        sync_topic: TopicId,
    ) -> Result<Self, SyncError> {
        let sync_store = SyncStore::new();
        let topic_map = InterpTopicMap::new();

        let log_sync =
            LogSync::builder(sync_store.clone(), topic_map.clone(), endpoint, gossip)
                .spawn()
                .await
                .map_err(|e| SyncError::Sync(e.to_string()))?;

        // Register our own public key in the topic map so we advertise our log
        // to peers during set reconciliation.
        topic_map.add_author(panda_key.public_key()).await;

        let handle: InterpSyncHandle = log_sync
            .stream(sync_topic, true)
            .await
            .map_err(|e| SyncError::Sync(e.to_string()))?;

        let (recv_tx, recv_rx) = mpsc::channel(256);

        // Background task: translate LogSync events → ReceivedInterp + ZodiaStore inserts.
        let mut subscription = handle
            .subscribe()
            .await
            .map_err(|e| SyncError::Sync(e.to_string()))?;

        let topic_map_bg = topic_map.clone();
        let zodia_store_bg = Arc::clone(&zodia_store);

        tokio::spawn(async move {
            while let Some(result) = subscription.next().await {
                let from_sync = match result {
                    Ok(fs) => fs,
                    Err(e) => {
                        warn!("sync subscription error: {e}");
                        continue;
                    }
                };

                match from_sync.event {
                    TopicLogSyncEvent::Operation(op) => {
                        let author_pk_bytes: [u8; 32] = *op.header.public_key.as_bytes();

                        // Register the author so we can relay their ops to future peers.
                        topic_map_bg.add_author(op.header.public_key).await;

                        let body_bytes = match &op.body {
                            Some(b) => b.to_bytes(),
                            None => {
                                debug!("sync: received operation without body, skipping");
                                continue;
                            }
                        };

                        let payload = match InterpPayload::decode(&body_bytes) {
                            Some(p) => p,
                            None => {
                                warn!("sync: failed to decode InterpPayload");
                                continue;
                            }
                        };

                        if payload.author_sig.len() != 64 {
                            warn!("sync: author_sig wrong length ({})", payload.author_sig.len());
                            continue;
                        }
                        let mut sig_arr = [0u8; 64];
                        sig_arr.copy_from_slice(&payload.author_sig);

                        let zodia_store = zodia_store_bg.clone();
                        let interp_key = payload.interp_key.clone();
                        let body_text = payload.body.clone();

                        let inserted = tokio::task::spawn_blocking(move || {
                            let store = zodia_store.lock().unwrap();
                            store.insert_received(&interp_key, &body_text, &author_pk_bytes, &sig_arr)
                        })
                        .await;

                        match inserted {
                            Ok(Ok(true)) => {
                                debug!(
                                    key = %payload.interp_key,
                                    "sync: new interpretation received"
                                );
                                let _ = recv_tx.send(ReceivedInterp {
                                    interp_key: payload.interp_key,
                                    body: payload.body,
                                    author_pk: author_pk_bytes,
                                    author_sig: sig_arr,
                                }).await;
                            }
                            Ok(Ok(false)) => {
                                debug!(key = %payload.interp_key, "sync: duplicate, skipped");
                            }
                            Ok(Err(StoreError::InvalidSignature)) => {
                                warn!(key = %payload.interp_key, "sync: invalid sig, discarded");
                            }
                            Ok(Err(e)) => {
                                warn!(key = %payload.interp_key, "sync: store error: {e}");
                            }
                            Err(e) => {
                                warn!("sync: spawn_blocking panic: {e}");
                            }
                        }
                    }
                    TopicLogSyncEvent::SyncStarted(_) => {
                        debug!(
                            remote = %hex::encode(&from_sync.remote.as_bytes()[..4]),
                            "sync: catch-up started"
                        );
                    }
                    TopicLogSyncEvent::SyncFinished(_) => {
                        debug!(
                            remote = %hex::encode(&from_sync.remote.as_bytes()[..4]),
                            "sync: catch-up finished"
                        );
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            panda_key,
            sync_store,
            handle,
            received: recv_rx,
        })
    }

    /// Publish a locally authored interpretation to the p2panda log.
    ///
    /// Callers must have already inserted the entry into `ZodiaStore` via
    /// `insert_signed`.  This method adds the operation to the p2panda log
    /// so that it will be propagated to peers via gossip and catch-up sync.
    ///
    /// `author_sig` is the Zodia-level ed25519 signature already stored in
    /// `ZodiaStore` — 64 bytes.
    pub async fn publish(
        &mut self,
        interp_key: &str,
        body: &str,
        author_sig: &[u8; 64],
    ) -> Result<(), SyncError> {
        let payload = InterpPayload {
            interp_key: interp_key.to_owned(),
            body: body.to_owned(),
            author_sig: author_sig.to_vec(),
        };
        let payload_bytes = payload.encode();

        // Determine the next sequence number and backlink from our log.
        let (seq_num, backlink) = self
            .sync_store
            .latest_operation(&self.panda_key.public_key(), &INTERP_LOG_ID)
            .await
            .unwrap_or(None)
            .map(|(header, _)| (header.seq_num + 1, Some(header.hash())))
            .unwrap_or((0, None));

        let body_op = Body::new(&payload_bytes);

        let mut header = Header::<()> {
            version: 1,
            public_key: self.panda_key.public_key(),
            signature: None,
            payload_size: body_op.size(),
            payload_hash: Some(body_op.hash()),
            timestamp: unix_secs(),
            seq_num,
            backlink,
            previous: vec![],
            extensions: (),
        };
        header.sign(&self.panda_key);
        let header_bytes = header.to_bytes();
        let op_hash = header.hash();

        // Insert into our in-memory log.
        self.sync_store
            .insert_operation(op_hash, &header, Some(&body_op), &header_bytes, &INTERP_LOG_ID)
            .await
            .map_err(|e| SyncError::Sync(e.to_string()))?;

        // Broadcast to connected peers via gossip.
        let op = Operation {
            hash: op_hash,
            header,
            body: Some(body_op),
        };
        self.handle
            .publish(op)
            .await
            .map_err(|e| SyncError::Sync(e.to_string()))?;

        Ok(())
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
