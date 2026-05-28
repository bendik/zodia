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
//! # Storage
//!
//! Operations are persisted in a dedicated SQLite file alongside the main
//! `interpretations.db` — this is the `p2panda-store::SqliteStore` that
//! `LogSync` uses for both `LogStore` and `TopicStore` duties.  Crashing
//! mid-sync no longer loses the local log; the new node will reuse it.

use std::path::Path;

use futures_util::StreamExt;
use p2panda_core::{Body, Header, Operation, SigningKey, Timestamp, Topic};
use p2panda_net::sync::LogSync;
use p2panda_net::{Endpoint, Gossip};
use p2panda_store::logs::LogStore;
use p2panda_store::operations::OperationStore;
use p2panda_store::{SqliteStore, SqliteStoreBuilder};
use p2panda_sync::protocols::TopicLogSyncEvent;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
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
    pub body:       String,
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

// ── sync node ─────────────────────────────────────────────────────────────────

/// Error type for sync operations.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("p2panda store: {0}")]
    PandaStore(String),
    #[error("p2panda sync: {0}")]
    Sync(String),
    #[error("payload encode/decode failed")]
    Payload,
}

/// A decoded interpretation that arrived via LogSync from a remote peer.
#[derive(Debug, Clone)]
pub struct ReceivedInterp {
    pub interp_key: String,
    pub body:       String,
    pub author_pk:  [u8; 32],
    pub author_sig: [u8; 64],
}

/// The live sync handle.
///
/// Keeps the p2panda LogSync machinery alive and mediates between the
/// application and the sync layer.
pub struct ZodiaSyncNode {
    /// p2panda signing key — same bytes as the Zodia identity `SigningKey`.
    signing_key: SigningKey,
    /// File-backed p2panda operation store.
    sync_store:  SqliteStore,
    /// LogSync handle for our single sync topic.
    handle:      p2panda_net::sync::SyncHandle<Operation<()>, TopicLogSyncEvent<()>>,
    /// Interpretations received from remote peers, ready for the app to consume.
    pub received: mpsc::Receiver<ReceivedInterp>,
}

impl ZodiaSyncNode {
    /// Spawn the sync node.
    ///
    /// * `signing_key` — the local identity p2panda `SigningKey`
    /// * `endpoint`    — clone of `ZodiaNetwork`'s iroh endpoint
    /// * `gossip`      — clone of `ZodiaNetwork`'s gossip engine
    /// * `zodia_store` — shared handle to the application `ZodiaStore` for
    ///                   persisting received interpretations
    /// * `sync_topic`  — the sync topic (use `Topic::from(topic_key_global().0)`)
    /// * `store_dir`   — directory in which the sync-store SQLite file lives
    pub async fn spawn(
        signing_key:  SigningKey,
        endpoint:     Endpoint,
        gossip:       Gossip,
        zodia_store:  ZodiaStore,
        sync_topic:   Topic,
        store_dir:    &Path,
    ) -> Result<Self, SyncError> {
        // ── persistent operation store ────────────────────────────────────────
        let sync_db_path = store_dir.join("sync_log.db");
        let url = format!("sqlite://{}", sync_db_path.display());
        let sync_store: SqliteStore = SqliteStoreBuilder::new()
            .database_url(&url)
            .build()
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        // ── LogSync ───────────────────────────────────────────────────────────
        let log_sync = LogSync::<_, u64, ()>::builder(sync_store.clone(), endpoint, gossip)
            .spawn()
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        let handle = log_sync
            .stream(sync_topic, true)
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        let (recv_tx, recv_rx) = mpsc::channel(256);

        // ── subscription background task ──────────────────────────────────────
        let mut subscription = handle
            .subscribe()
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        let zodia_store_bg = zodia_store.clone();

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
                    TopicLogSyncEvent::OperationReceived { operation, .. } => {
                        let author_pk_bytes: [u8; 32] =
                            *operation.header.verifying_key.as_bytes();

                        let body_bytes = match &operation.body {
                            Some(b) => b.to_bytes(),
                            None => {
                                debug!("sync: operation without body, skipping");
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

                        let interp_key = payload.interp_key.clone();
                        let body_text  = payload.body.clone();

                        match zodia_store_bg
                            .insert_received(&interp_key, &body_text, &author_pk_bytes, &sig_arr)
                            .await
                        {
                            Ok(true) => {
                                debug!(key = %payload.interp_key, "sync: new interpretation received");
                                let _ = recv_tx.send(ReceivedInterp {
                                    interp_key: payload.interp_key,
                                    body:       payload.body,
                                    author_pk:  author_pk_bytes,
                                    author_sig: sig_arr,
                                }).await;
                            }
                            Ok(false) => {
                                debug!(key = %payload.interp_key, "sync: duplicate, skipped");
                            }
                            Err(StoreError::InvalidSignature) => {
                                warn!(key = %payload.interp_key, "sync: invalid sig, discarded");
                            }
                            Err(e) => {
                                warn!(key = %payload.interp_key, "sync: store error: {e}");
                            }
                        }
                    }
                    TopicLogSyncEvent::SyncStarted { .. } => {
                        debug!(
                            remote = %hex::encode(&from_sync.remote.as_bytes()[..4]),
                            "sync: catch-up started"
                        );
                    }
                    TopicLogSyncEvent::SyncFinished { .. } => {
                        debug!(
                            remote = %hex::encode(&from_sync.remote.as_bytes()[..4]),
                            "sync: catch-up finished"
                        );
                    }
                    TopicLogSyncEvent::Failed { error } => {
                        warn!(remote = %hex::encode(&from_sync.remote.as_bytes()[..4]),
                              "sync session failed: {error}");
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            signing_key,
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
            body:       body.to_owned(),
            author_sig: author_sig.to_vec(),
        };
        let payload_bytes = payload.encode();

        // Determine the next sequence number + backlink from our log tip.
        let latest: Option<Operation<()>> = self.sync_store
            .get_latest_entry(&self.signing_key.verifying_key(), &INTERP_LOG_ID)
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        let (seq_num, backlink) = match latest {
            Some(op) => (op.header.seq_num + 1, Some(op.header.hash())),
            None     => (0, None),
        };

        let body_op = Body::new(&payload_bytes);

        let mut header = Header::<()> {
            version:       1,
            verifying_key: self.signing_key.verifying_key(),
            signature:     None,
            payload_size:  body_op.size(),
            payload_hash:  Some(body_op.hash()),
            timestamp:     Timestamp::now(),
            seq_num,
            backlink,
            extensions:    (),
        };
        header.sign(&self.signing_key);
        let op_hash = header.hash();

        let operation = Operation {
            hash:   op_hash,
            header,
            body:   Some(body_op),
        };

        // Persist locally so the next publish picks up the right backlink and
        // peers that catch us mid-publish can complete the log.
        self.sync_store
            .insert_operation(&op_hash, &operation, &INTERP_LOG_ID)
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        // Broadcast to connected peers via gossip.
        self.handle
            .publish(operation)
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        Ok(())
    }
}
