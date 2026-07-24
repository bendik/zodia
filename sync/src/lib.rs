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
use p2panda_core::{Body, Header, Operation, SigningKey, Timestamp, Topic, VerifyingKey};
use p2panda_net::sync::LogSync;
use p2panda_net::{Endpoint, Gossip};
use p2panda_store::logs::LogStore;
use p2panda_store::operations::OperationStore;
use p2panda_store::{SqliteStore, SqliteStoreBuilder, Transaction};
use p2panda_sync::protocols::TopicLogSyncEvent;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use zodia_ops::{DocOp, InterpOp};

// ── sync event ────────────────────────────────────────────────────────────────

/// Everything the subscription task wants to tell the app about: data
/// (one received operation) and lifecycle (sync sessions starting,
/// finishing, failing).
#[derive(Debug)]
pub enum SyncEvent {
    /// A peer's operation came down the wire.  Will get decoded + materialised
    /// by `zodia-pipeline` downstream.
    OperationReceived(Box<Operation<()>>),
    /// A catch-up sync session opened with `remote`.
    SyncStarted {
        remote: VerifyingKey,
    },
    /// A catch-up sync session with `remote` finished.  `received_ops` is the
    /// running total of ops we have received from this session and any live
    /// gossip after it — a usable proxy for "are we caught up".
    SyncFinished {
        remote:         VerifyingKey,
        received_ops:   u64,
        received_bytes: u64,
    },
    /// A sync session with `remote` failed mid-flight.
    Failed {
        remote: VerifyingKey,
        error:  String,
    },
}

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
    /// Mixed-purpose channel: operation arrivals plus lifecycle events
    /// (session start / finish / failure).  Operations feed the pipeline;
    /// lifecycle events drive UI sync-status indicators.
    pub inbound: mpsc::Receiver<SyncEvent>,
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

        let (ev_tx, ev_rx) = mpsc::channel::<SyncEvent>(256);

        // ── subscription background task ──────────────────────────────────────
        //
        // Thin forwarder: every `OperationReceived` becomes a SyncEvent::
        // OperationReceived; lifecycle events become SyncEvent variants so
        // the app can drive sync-status UI off them.
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

                let remote = from_sync.remote;
                let remote_tag = hex::encode(&remote.as_bytes()[..4]);
                let event = match from_sync.event {
                    TopicLogSyncEvent::OperationReceived { operation, .. } => {
                        SyncEvent::OperationReceived(operation)
                    }
                    TopicLogSyncEvent::SyncStarted { .. } => {
                        debug!(remote = %remote_tag, "sync: catch-up started");
                        SyncEvent::SyncStarted { remote }
                    }
                    TopicLogSyncEvent::SyncFinished { metrics } => {
                        debug!(remote = %remote_tag, "sync: catch-up finished");
                        SyncEvent::SyncFinished {
                            remote,
                            received_ops:   metrics.received_operations(),
                            received_bytes: metrics.received_bytes(),
                        }
                    }
                    TopicLogSyncEvent::Failed { error } => {
                        warn!(remote = %remote_tag, "sync session failed: {error}");
                        SyncEvent::Failed { remote, error }
                    }
                    _ => continue,
                };

                if ev_tx.send(event).await.is_err() {
                    debug!("inbound sync channel closed, stopping subscription pump");
                    break;
                }
            }
        });

        Ok(Self {
            signing_key,
            sync_store,
            handle,
            inbound: ev_rx,
        })
    }

    /// Publish a locally authored `InterpOp` to the p2panda log.  See
    /// [`Self::publish_doc`] for the Phase F-collab `DocOp` equivalent.
    pub async fn publish(&mut self, op: InterpOp) -> Result<(), SyncError> {
        self.publish_bytes(op.encode()).await
    }

    /// Publish a locally authored `DocOp` (Phase F-collab) to the same
    /// log.  Same backlink/seq/sign mechanics as [`Self::publish`].
    pub async fn publish_doc(&mut self, op: DocOp) -> Result<(), SyncError> {
        self.publish_bytes(op.encode()).await
    }

    async fn publish_bytes(&mut self, payload_bytes: Vec<u8>) -> Result<(), SyncError> {
        // p2panda-store's `insert_operation` runs inside a transaction
        // started by `begin()`.  Without it, the store returns
        // `TransactionMissing` ("tried to interact with inexistant
        // transaction").  We open a single transaction spanning the
        // latest-entry read + insert, commit before publishing on the
        // network so the local log tip is authoritative.
        let permit = self.sync_store
            .begin()
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

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

        if let Err(e) = self.sync_store
            .insert_operation(&op_hash, &operation, &INTERP_LOG_ID)
            .await
        {
            // Rollback drops the permit and frees the semaphore so the
            // next publish can begin a new txn.
            let _ = self.sync_store.rollback(permit).await;
            return Err(SyncError::PandaStore(e.to_string()));
        }

        self.sync_store
            .commit(permit)
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        self.handle
            .publish(operation)
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        Ok(())
    }
}
