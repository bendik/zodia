//! Offline-first interpretation sync driver.
//!
//! `ZodiaSyncNode` owns the `p2panda-net::LogSync` machinery and exposes
//! two simple channels to the app layer:
//!
//! * `inbound_ops` — every received `Operation<()>`, raw.  The app feeds
//!   these into a `zodia-pipeline::ZodiaPipeline` for decoding, ordering,
//!   access-control, materialisation.
//! * `publish(op: InterpOp)` — encode the canonical Zodia op into a body,
//!   build + sign the p2panda header, persist to the local log, broadcast
//!   via LogSync.
//!
//! The Zodia-level "author_sig over BLAKE3(key||body)" that the old
//! `InterpPayload` carried is gone: the p2panda header signature IS the
//! authentication for LogSync-replicated ops, and `zodia-store::insert_from_op`
//! trusts that the caller (the pipeline) verified the chain.
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
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use zodia_ops::InterpOp;

// ── log id ────────────────────────────────────────────────────────────────────

/// Each author has exactly one log containing all their interpretations.
const INTERP_LOG_ID: u64 = 0;

// ── errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("p2panda store: {0}")]
    PandaStore(String),
    #[error("p2panda sync: {0}")]
    Sync(String),
}

// ── sync node ─────────────────────────────────────────────────────────────────

/// The live sync handle.  Keeps the p2panda LogSync machinery alive and
/// exposes a raw `Operation<()>` channel for the app's pipeline to consume.
pub struct ZodiaSyncNode {
    /// p2panda signing key — same bytes as the Zodia identity `SigningKey`.
    signing_key: SigningKey,
    /// File-backed p2panda operation store.
    sync_store:  SqliteStore,
    /// LogSync handle for our single sync topic.
    handle:      p2panda_net::sync::SyncHandle<Operation<()>, TopicLogSyncEvent<()>>,
    /// Raw operations received from remote peers, ready for the app's
    /// `ZodiaPipeline` to consume.
    pub inbound_ops: mpsc::Receiver<Operation<()>>,
}

impl ZodiaSyncNode {
    /// Spawn the sync node.
    ///
    /// * `signing_key` — the local identity p2panda `SigningKey`
    /// * `endpoint`    — clone of `ZodiaNetwork`'s iroh endpoint
    /// * `gossip`      — clone of `ZodiaNetwork`'s gossip engine
    /// * `sync_topic`  — the sync topic (use `Topic::from(topic_key_global().0)`)
    /// * `store_dir`   — directory in which the sync-store SQLite file lives
    pub async fn spawn(
        signing_key:  SigningKey,
        endpoint:     Endpoint,
        gossip:       Gossip,
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

        let (op_tx, op_rx) = mpsc::channel::<Operation<()>>(256);

        // ── subscription background task ──────────────────────────────────────
        //
        // The task is intentionally thin: forward every received operation
        // to the channel and log non-operation lifecycle events.  All
        // decoding / verification / storage decisions happen downstream in
        // the app's `ZodiaPipeline`.
        let mut subscription = handle
            .subscribe()
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        tokio::spawn(async move {
            while let Some(result) = subscription.next().await {
                let from_sync = match result {
                    Ok(fs) => fs,
                    Err(e) => {
                        warn!("sync subscription error: {e}");
                        continue;
                    }
                };

                let remote_tag = hex::encode(&from_sync.remote.as_bytes()[..4]);
                match from_sync.event {
                    TopicLogSyncEvent::OperationReceived { operation, .. } => {
                        // `Box<Operation<()>>` → owned `Operation<()>`.
                        let op = *operation;
                        if op_tx.send(op).await.is_err() {
                            debug!("inbound_ops channel closed, stopping subscription pump");
                            break;
                        }
                    }
                    TopicLogSyncEvent::SyncStarted { .. } => {
                        debug!(remote = %remote_tag, "sync: catch-up started");
                    }
                    TopicLogSyncEvent::SyncFinished { .. } => {
                        debug!(remote = %remote_tag, "sync: catch-up finished");
                    }
                    TopicLogSyncEvent::Failed { error } => {
                        warn!(remote = %remote_tag, "sync session failed: {error}");
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            signing_key,
            sync_store,
            handle,
            inbound_ops: op_rx,
        })
    }

    /// Publish a locally authored `InterpOp` to the p2panda log.
    ///
    /// Encodes the op, builds + signs a p2panda header, persists locally
    /// (so the next publish picks up the right backlink and crash-mid-publish
    /// recovers), then broadcasts via LogSync.
    pub async fn publish(&mut self, op: InterpOp) -> Result<(), SyncError> {
        let payload_bytes = op.encode();

        // Determine the next sequence number + backlink from our log tip.
        let latest: Option<Operation<()>> = self.sync_store
            .get_latest_entry(&self.signing_key.verifying_key(), &INTERP_LOG_ID)
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        let (seq_num, backlink) = match latest {
            Some(prev) => (prev.header.seq_num + 1, Some(prev.header.hash())),
            None       => (0, None),
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

        self.sync_store
            .insert_operation(&op_hash, &operation, &INTERP_LOG_ID)
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        self.handle
            .publish(operation)
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        Ok(())
    }
}
