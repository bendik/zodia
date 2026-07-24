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

use std::collections::HashMap;
use std::path::Path;

use futures_util::StreamExt;
use p2panda_core::{Body, Header, Operation, SigningKey, Timestamp, Topic, VerifyingKey};
use p2panda_net::sync::{LogSync, SyncHandle};
use p2panda_net::{Endpoint, Gossip};
use p2panda_store::logs::LogStore;
use p2panda_store::operations::OperationStore;
use p2panda_store::topics::TopicStore;
use p2panda_store::{SqliteStore, SqliteStoreBuilder, Transaction};
use p2panda_sync::protocols::TopicLogSyncEvent;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use zodia_core::topic_key_for_interp;
use zodia_ops::{DocOp, InterpOp, log_id_for_key};

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

/// Legacy log: every pre-Phase-C-2 `InterpOp` an author ever published.
/// Signed operations can't be re-homed to a derived `log_id` (the p2panda
/// header signature covers `log_id`), so this stays the permanent address
/// of pre-migration history — `InterpOp::publish` still targets it.  New
/// `DocOp` writes use `zodia_ops::log_id_for_key` instead (see
/// `docs/prd/granular-topic-subscription.md`).
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
///
/// Phase C-2: holds one `SyncHandle` per subscribed topic rather than a
/// single fixed one.  `global_topic` (legacy `InterpOp` traffic, log 0)
/// is always subscribed; per-key topics for `DocOp` traffic come and go
/// as the app calls `subscribe`/`unsubscribe`.
pub struct ZodiaSyncNode {
    /// p2panda signing key — same bytes as the Zodia identity `SigningKey`.
    signing_key:  SigningKey,
    /// File-backed p2panda operation store.
    sync_store:   SqliteStore,
    /// Shared LogSync engine — `.stream()` opens a new topic subscription
    /// without needing a fresh endpoint/gossip pair.
    log_sync:     LogSync<SqliteStore, u64, ()>,
    /// The always-on legacy topic; `publish` (InterpOp) targets this one.
    global_topic: Topic,
    /// Forwarder-task sender, cloned into each newly opened topic's pump.
    ev_tx:        mpsc::Sender<SyncEvent>,
    /// Live handles keyed by topic. Dropping an entry ends that topic's
    /// sync session (`SyncHandle::drop` sends `ToSyncManager::Close`).
    handles:      HashMap<Topic, SyncHandle<Operation<()>, TopicLogSyncEvent<()>>>,
    /// Mixed-purpose channel: operation arrivals plus lifecycle events
    /// (session start / finish / failure).  Operations feed the pipeline;
    /// lifecycle events drive UI sync-status indicators.
    pub inbound: mpsc::Receiver<SyncEvent>,
}

impl ZodiaSyncNode {
    /// Spawn the sync node, opening `sync_topic` (the legacy global topic)
    /// immediately.
    ///
    /// * `signing_key` — the local identity p2panda `SigningKey`
    /// * `endpoint`    — clone of `ZodiaNetwork`'s iroh endpoint
    /// * `gossip`      — clone of `ZodiaNetwork`'s gossip engine
    /// * `sync_topic`  — the legacy sync topic (use `Topic::from(topic_key_global().0)`)
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

        let (ev_tx, ev_rx) = mpsc::channel::<SyncEvent>(256);

        let mut node = Self {
            signing_key,
            sync_store,
            log_sync,
            global_topic: sync_topic,
            ev_tx,
            handles: HashMap::new(),
            inbound: ev_rx,
        };
        node.open_topic(sync_topic).await?;

        Ok(node)
    }

    /// Subscribe to a key's per-key topic (Phase C-2).  Idempotent — a
    /// key already subscribed is a no-op.  `DocOp` traffic for `interp_key`
    /// only reaches this device while subscribed.
    pub async fn subscribe(&mut self, interp_key: &str) -> Result<(), SyncError> {
        self.open_topic(Topic::from(topic_key_for_interp(interp_key).0)).await
    }

    /// Unsubscribe from a key's per-key topic.  No-op if not subscribed.
    /// Dropping the handle ends the sync session (`SyncHandle::drop`).
    pub fn unsubscribe(&mut self, interp_key: &str) {
        let topic = Topic::from(topic_key_for_interp(interp_key).0);
        self.handles.remove(&topic);
    }

    /// Open (if not already) a LogSync stream for `topic` and start its
    /// forwarder task.  Shared by `spawn`'s global-topic bootstrap and
    /// `subscribe`'s per-key topics.
    async fn open_topic(&mut self, topic: Topic) -> Result<(), SyncError> {
        if self.handles.contains_key(&topic) {
            return Ok(());
        }

        let handle = self.log_sync
            .stream(topic, true)
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        // Thin forwarder: every `OperationReceived` becomes a SyncEvent::
        // OperationReceived; lifecycle events become SyncEvent variants so
        // the app can drive sync-status UI off them.
        let mut subscription = handle
            .subscribe()
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;
        let ev_tx = self.ev_tx.clone();

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

        self.handles.insert(topic, handle);
        Ok(())
    }

    /// Publish a locally authored `InterpOp` to the legacy global log
    /// (log 0). See [`Self::publish_doc`] for the Phase F-collab `DocOp`
    /// equivalent, which routes to a per-key log/topic instead.
    pub async fn publish(&mut self, op: InterpOp) -> Result<(), SyncError> {
        let topic = self.global_topic;
        self.publish_bytes(op.encode(), INTERP_LOG_ID, topic).await
    }

    /// Publish a locally authored `DocOp` (Phase F-collab) to its key's
    /// per-key log/topic (Phase C-2), subscribing first if not already —
    /// publishing into a topic you're not on isn't meaningful, so this
    /// implicitly opens it, mirroring "the page you're editing is already
    /// subscribed" from the app-layer lifecycle policy.
    pub async fn publish_doc(&mut self, op: DocOp) -> Result<(), SyncError> {
        let interp_key = op.interp_key().to_string();
        let log_id = log_id_for_key(&interp_key);
        let topic = Topic::from(topic_key_for_interp(&interp_key).0);
        self.open_topic(topic).await?;
        self.publish_bytes(op.encode(), log_id, topic).await
    }

    async fn publish_bytes(
        &mut self,
        payload_bytes: Vec<u8>,
        log_id:        u64,
        topic:         Topic,
    ) -> Result<(), SyncError> {
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
            .get_latest_entry(&self.signing_key.verifying_key(), &log_id)
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
            .insert_operation(&op_hash, &operation, &log_id)
            .await
        {
            // Rollback drops the permit and frees the semaphore so the
            // next publish can begin a new txn.
            let _ = self.sync_store.rollback(permit).await;
            return Err(SyncError::PandaStore(e.to_string()));
        }

        // Register (topic, author, log_id) so peers who subscribe to this
        // topic *after* this op already exists can discover it during
        // catch-up (`TopicStore::associate` — without this, LogSync's
        // "local topic logs retrieved" query has nothing to find, and only
        // an already-open live session would ever see the op via the
        // separate live-forward path). `associate`'s internal `self.tx(..)`
        // requires an already-open transaction (same constraint as
        // `insert_operation`, see comment above), so this must run before
        // `commit`, not after.
        if let Err(e) = self.sync_store
            .associate(&topic, &self.signing_key.verifying_key(), &log_id)
            .await
        {
            let _ = self.sync_store.rollback(permit).await;
            return Err(SyncError::PandaStore(e.to_string()));
        }

        self.sync_store
            .commit(permit)
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        // `open_topic` (called by every publish path above) guarantees an
        // entry exists for `topic` by the time we get here.
        self.handles
            .get(&topic)
            .expect("publish_bytes called after open_topic")
            .publish(operation)
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        Ok(())
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────
//
// `ZodiaSyncNode` itself needs a live network (endpoint + gossip) to spawn,
// so it isn't unit-testable in isolation — see `zodia-sdk`'s real two-client
// networked test for that level. What *is* unit-testable without a network
// is the store contract `publish_bytes` depends on: these tests pin the
// transaction-bracketing behaviour whose violation caused the bug described
// in `docs/prd/granular-topic-subscription.md`'s "Bug found and fixed" note.

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_op(key: &SigningKey, payload: &[u8]) -> Operation<()> {
        let body = Body::new(payload);
        let mut header = Header::<()> {
            version:       1,
            verifying_key: key.verifying_key(),
            signature:     None,
            payload_size:  body.size(),
            payload_hash:  Some(body.hash()),
            timestamp:     Timestamp::now(),
            seq_num:       0,
            backlink:      None,
            extensions:    (),
        };
        header.sign(key);
        Operation { hash: header.hash(), header, body: Some(body) }
    }

    /// A peer must be able to discover our log for a topic via
    /// `TopicStore::resolve` immediately after we publish to it — that's
    /// what a subscriber's catch-up query relies on. `associate` has to
    /// run inside the same transaction as `insert_operation`, before
    /// `commit`, exactly as `publish_bytes` does above.
    #[tokio::test]
    async fn associate_inside_publish_transaction_is_discoverable_after_commit() {
        let store   = SqliteStore::temporary().await;
        let key     = SigningKey::generate();
        let topic   = Topic::from([7u8; 32]);
        let log_id: u64 = 42;
        let op      = signed_op(&key, b"hello");

        let permit = store.begin().await.expect("begin");
        store.insert_operation(&op.hash, &op, &log_id).await.expect("insert");
        store.associate(&topic, &key.verifying_key(), &log_id).await.expect("associate");
        store.commit(permit).await.expect("commit");

        let found: std::collections::BTreeMap<VerifyingKey, Vec<u64>> =
            store.resolve(&topic).await.expect("resolve");
        let logs = found.get(&key.verifying_key()).expect("author present in resolved map");
        assert_eq!(logs, &vec![log_id]);
    }

    /// `TopicStore::associate`'s own `self.tx(..)` requires an
    /// already-open transaction — calling it *after* `commit` (the bug's
    /// original shape) must fail loudly, not silently no-op. Pins that
    /// contract so a future refactor that reorders `publish_bytes`'s calls
    /// fails a test instead of quietly reintroducing the bug.
    #[tokio::test]
    async fn associate_after_commit_is_rejected_not_silently_dropped() {
        let store   = SqliteStore::temporary().await;
        let key     = SigningKey::generate();
        let topic   = Topic::from([7u8; 32]);
        let log_id: u64 = 42;
        let op      = signed_op(&key, b"hello");

        let permit = store.begin().await.expect("begin");
        store.insert_operation(&op.hash, &op, &log_id).await.expect("insert");
        store.commit(permit).await.expect("commit");

        let result = store.associate(&topic, &key.verifying_key(), &log_id).await;
        assert!(result.is_err(), "associate outside a transaction should error, not succeed silently");
    }

    /// `log_id_for_key` (from `zodia-ops`) plus `topic_key_for_interp`
    /// (from `zodia-core`) is the whole per-key routing contract
    /// `publish_doc` relies on — pin that two different keys land in two
    /// different (log, topic) pairs, and the same key is always the same
    /// pair (so re-publishing to a key reuses its existing log/topic).
    #[test]
    fn per_key_log_and_topic_derivation_is_stable_and_distinct() {
        let a = "natal:sun_trine_moon";
        let b = "natal:venus_square_pluto";

        assert_eq!(log_id_for_key(a), log_id_for_key(a));
        assert_ne!(log_id_for_key(a), log_id_for_key(b));

        let topic_a = topic_key_for_interp(a);
        let topic_b = topic_key_for_interp(b);
        assert_eq!(topic_a.as_bytes(), topic_key_for_interp(a).as_bytes());
        assert_ne!(topic_a.as_bytes(), topic_b.as_bytes());
    }
}
