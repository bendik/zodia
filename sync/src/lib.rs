//! Offline-first interpretation sync driver.
//!
//! `ZodiaSyncNode` owns the `p2panda-net::LogSync` machinery and exposes
//! two simple channels to the app layer:
//!
//! * `inbound_ops` — every received `Operation<OpExtensions>`, raw.  The app feeds
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
use p2panda_core::{Body, Hash, Header, Operation, SigningKey, Timestamp, Topic, VerifyingKey};
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

use zodia_circles::{
    Access, CircleError, CircleExtensions, CircleManager, CircleOperation,
    circle_directory_topic, persist_received_and_associate, topic_for_circle,
};
pub use zodia_circles::{Event as CircleEvent, SpaceId};
use zodia_core::topic_key_for_interp;
use zodia_ops::{DocOp, InterpOp, OpExtensions, log_id_for_key};

// ── sync event ────────────────────────────────────────────────────────────────

/// Everything the subscription task wants to tell the app about: data
/// (one received operation) and lifecycle (sync sessions starting,
/// finishing, failing).
#[derive(Debug)]
pub enum SyncEvent {
    /// A peer's operation came down the wire.  Will get decoded + materialised
    /// by `zodia-pipeline` downstream.
    OperationReceived(Box<Operation<OpExtensions>>),
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
    /// A circle's decrypted application message arrived — `plaintext` is
    /// exactly the bytes `share_to_circle` handed to `Space::publish` on
    /// the sending side (Zodia encodes it as `InterpOp`/`DocOp` CBOR, same
    /// as the public path). `op_id`/`author` identify the *circle*
    /// operation that carried the ciphertext, since the plaintext itself
    /// carries neither — see `zodia_pipeline::materialize_circle_content`,
    /// which this is meant to feed directly.
    CircleContentReceived {
        space_id:  SpaceId,
        op_id:     Hash,
        author:    VerifyingKey,
        plaintext: Vec<u8>,
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
    #[error("circle: {0}")]
    Circle(String),
}

impl From<CircleError> for SyncError {
    fn from(e: CircleError) -> Self {
        SyncError::Circle(e.to_string())
    }
}

// ── sync node ─────────────────────────────────────────────────────────────────

/// The live sync handle.  Keeps the p2panda LogSync machinery alive and
/// exposes a raw `Operation<OpExtensions>` channel for the app's pipeline to consume.
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
    log_sync:     LogSync<SqliteStore, u64, OpExtensions>,
    /// The always-on legacy topic; `publish` (InterpOp) targets this one.
    global_topic: Topic,
    /// Forwarder-task sender, cloned into each newly opened topic's pump.
    ev_tx:        mpsc::Sender<SyncEvent>,
    /// Live handles keyed by topic. Dropping an entry ends that topic's
    /// sync session (`SyncHandle::drop` sends `ToSyncManager::Close`).
    handles:      HashMap<Topic, SyncHandle<Operation<OpExtensions>, TopicLogSyncEvent<OpExtensions>>>,
    /// Mixed-purpose channel: operation arrivals plus lifecycle events
    /// (session start / finish / failure).  Operations feed the pipeline;
    /// lifecycle events drive UI sync-status indicators.
    pub inbound: mpsc::Receiver<SyncEvent>,
    /// Circle membership/encryption state (`p2panda-spaces::Manager`,
    /// wrapped by `zodia-circles`). Shares `sync_store` — see
    /// `docs/prd/circles.md`'s "Storage needs nothing new".
    circle_manager: CircleManager,
    /// A *second* LogSync engine, distinct from `log_sync` above —
    /// `LogSync<S, L, E>` is monomorphic in `E`, and circle operations
    /// (`Operation<CircleExtensions>`) are a different `E` than
    /// `OpExtensions`, so they can't share the same engine. Shares the same
    /// `Endpoint`/`Gossip`/`SqliteStore` as `log_sync` regardless — see
    /// `docs/prd/circles.md`'s "a second LogSync engine, not a new topic".
    circle_log_sync: LogSync<SqliteStore, u64, CircleExtensions>,
    /// Circle-topic equivalent of `handles`.
    circle_handles: HashMap<Topic, SyncHandle<Operation<CircleExtensions>, TopicLogSyncEvent<CircleExtensions>>>,
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

        // ── LogSync (InterpOp/DocOp) ───────────────────────────────────────────
        let log_sync = LogSync::<_, u64, OpExtensions>::builder(
            sync_store.clone(), endpoint.clone(), gossip.clone(),
        )
            .spawn()
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        // ── LogSync (circles) — a second engine, see the struct field docs
        // on `circle_log_sync` for why one engine can't serve both ─────────
        let circle_log_sync = LogSync::<_, u64, CircleExtensions>::builder(
            sync_store.clone(), endpoint, gossip,
        )
            .spawn()
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        let circle_secret_path = store_dir.join("circle_identity_secret.cbor");
        let circle_manager = zodia_circles::new_manager(
            &circle_secret_path, sync_store.clone(), signing_key.clone(),
        )?;

        let (ev_tx, ev_rx) = mpsc::channel::<SyncEvent>(256);

        let mut node = Self {
            signing_key,
            sync_store,
            log_sync,
            global_topic: sync_topic,
            ev_tx,
            handles: HashMap::new(),
            inbound: ev_rx,
            circle_manager,
            circle_log_sync,
            circle_handles: HashMap::new(),
        };
        node.open_topic(sync_topic, INTERP_LOG_ID).await?;
        node.open_circle_topic(circle_directory_topic()).await?;

        // Announce our own key bundle on the directory topic so any peer we
        // meet can discover it — the prerequisite for being added to a
        // circle (see `zodia_circles::circle_directory_topic`'s doc comment).
        let key_bundle_op = node.circle_manager.key_bundle_message().await
            .map_err(|e| SyncError::Circle(e.to_string()))?;
        node.broadcast_circle_op(circle_directory_topic(), key_bundle_op)?;

        Ok(node)
    }

    /// Subscribe to a key's per-key topic (Phase C-2).  Idempotent — a
    /// key already subscribed is a no-op.  `DocOp` traffic for `interp_key`
    /// only reaches this device while subscribed.
    pub async fn subscribe(&mut self, interp_key: &str) -> Result<(), SyncError> {
        let topic = Topic::from(topic_key_for_interp(interp_key).0);
        self.open_topic(topic, log_id_for_key(interp_key)).await
    }

    /// Unsubscribe from a key's per-key topic.  No-op if not subscribed.
    /// Dropping the handle ends the sync session (`SyncHandle::drop`).
    pub fn unsubscribe(&mut self, interp_key: &str) {
        let topic = Topic::from(topic_key_for_interp(interp_key).0);
        self.handles.remove(&topic);
    }

    /// Permanently delete locally-stored operations older than `cutoff`,
    /// except any authored by this device's own identity (own contributions
    /// are never pruned, regardless of age — see [`prune_older_than`]).
    /// Local-storage-only: peers who still have the pruned history are
    /// unaffected, and this device can re-receive it later via normal
    /// catch-up sync if it re-subscribes to the relevant topic. Returns the
    /// number of operations removed.
    pub async fn prune_older_than(&self, cutoff: Timestamp) -> Result<u64, SyncError> {
        prune_older_than(&self.sync_store, &self.signing_key.verifying_key(), cutoff).await
    }

    /// Open (if not already) a LogSync stream for `topic` and start its
    /// forwarder task.  Shared by `spawn`'s global-topic bootstrap and
    /// `subscribe`'s per-key topics. `log_id` is uniform across *every*
    /// author on this topic (log 0 for the legacy global topic, the key's
    /// derived log for a per-key topic — never derived from a specific
    /// author), so the forwarder below can use it to persist any received
    /// operation regardless of who published it.
    async fn open_topic(&mut self, topic: Topic, log_id: u64) -> Result<(), SyncError> {
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
        let sync_store = self.sync_store.clone();

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
                        // Persist what we received, not just what we
                        // authored — otherwise this device can never
                        // re-serve it to a third peer, and it vanishes the
                        // moment the process exits (see this fix's doc
                        // comment on `store_and_associate` for the bug this
                        // closes).
                        let author = operation.header.verifying_key;
                        if let Err(e) = store_and_associate(
                            &sync_store, topic, &author, log_id, &operation.hash, &operation,
                        ).await {
                            warn!(remote = %remote_tag, "failed to persist received operation: {e}");
                        }
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
                            received_ops:   metrics.received_operations() as u64,
                            received_bytes: metrics.received_bytes() as u64,
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

    /// Circle-topic equivalent of `open_topic` — a separate method (not a
    /// generic-over-extensions-type shared one) because it drives the
    /// *other* LogSync engine (`circle_log_sync`, not `log_sync`) and hands
    /// received operations to `circle_manager.process_persisted` rather
    /// than just forwarding raw bytes for the pipeline to decode later —
    /// circle content is already fully handled (persisted, decrypted if
    /// applicable) by the time it becomes a `SyncEvent`.
    async fn open_circle_topic(&mut self, topic: Topic) -> Result<(), SyncError> {
        if self.circle_handles.contains_key(&topic) {
            return Ok(());
        }

        let handle = self.circle_log_sync
            .stream(topic, true)
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        let mut subscription = handle
            .subscribe()
            .await
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;
        let ev_tx = self.ev_tx.clone();
        let sync_store = self.sync_store.clone();
        let circle_manager = self.circle_manager.clone();

        tokio::spawn(async move {
            while let Some(result) = subscription.next().await {
                let from_sync = match result {
                    Ok(fs) => fs,
                    Err(e) => {
                        warn!("circle sync subscription error: {e}");
                        continue;
                    }
                };

                let TopicLogSyncEvent::OperationReceived { operation, .. } = from_sync.event
                else {
                    continue;
                };

                let author = operation.header.verifying_key;
                let circle_op = CircleOperation(*operation);

                if let Err(e) = persist_received_and_associate(
                    &sync_store, topic, &author, &circle_op,
                ).await {
                    warn!("failed to persist received circle operation: {e}");
                    continue;
                }

                let events = match zodia_circles::process_and_persist(&circle_manager, &sync_store, &circle_op).await {
                    Ok(events) => events,
                    Err(e) => {
                        warn!("failed to process received circle operation: {e}");
                        continue;
                    }
                };

                for event in events {
                    let sync_event = match event {
                        CircleEvent::Application { space_id, data } => {
                            SyncEvent::CircleContentReceived {
                                space_id,
                                op_id:  circle_op.0.hash,
                                author,
                                plaintext: data,
                            }
                        }
                        other => {
                            debug!(?other, "circle event (membership/key-bundle), no content to surface yet");
                            continue;
                        }
                    };
                    if ev_tx.send(sync_event).await.is_err() {
                        debug!("inbound sync channel closed, stopping circle subscription pump");
                        return;
                    }
                }
            }
        });

        self.circle_handles.insert(topic, handle);
        Ok(())
    }

    /// Broadcast an already-signed-and-persisted circle op (every
    /// `CircleManager`/`CircleSpace` method that returns one already
    /// persisted it via `ZodiaForge` — this is just the missing broadcast
    /// step, same division of labour `publish_bytes` has for regular ops).
    fn broadcast_circle_op(&mut self, topic: Topic, op: CircleOperation) -> Result<(), SyncError> {
        self.circle_handles
            .get(&topic)
            .expect("broadcast_circle_op called after open_circle_topic")
            .publish(op.0)
            .map_err(|e| SyncError::Sync(format!("{e:?}")))
    }

    /// Create a new circle with `initial_members` (beyond ourselves, who is
    /// always added with `Access::manage()` automatically — see
    /// `Manager::create_space`'s own doc comment). Returns the new circle's
    /// id, needed for every subsequent `invite_to_circle`/`share_to_circle`/
    /// `circle_members`/`revoke_from_circle` call.
    pub async fn create_circle(
        &mut self,
        initial_members: &[(VerifyingKey, Access<()>)],
    ) -> Result<SpaceId, SyncError> {
        let circle_id = SpaceId::digest(self.signing_key.verifying_key().to_hex().as_bytes())
            .to_owned();
        // Salt with the current time so the same identity can create more
        // than one circle without colliding on the same SpaceId.
        let circle_id = SpaceId::digest(
            [circle_id.as_bytes().as_slice(), &Timestamp::now().to_string().into_bytes()].concat(),
        );

        let topic = topic_for_circle(circle_id);
        self.open_circle_topic(topic).await?;

        let messages = zodia_circles::create_circle(
            &self.circle_manager, &self.sync_store, circle_id, initial_members,
        ).await?;
        for message in messages {
            self.broadcast_circle_op(topic, message)?;
        }

        Ok(circle_id)
    }

    /// Grant `member` `access` in `circle_id`. `member` must already be
    /// discoverable — i.e. this device has processed a `KeyBundle` message
    /// from them at some point, which happens automatically for any peer
    /// this device has ever synced the directory topic with (see
    /// `spawn`'s key-bundle announcement and `circle_directory_topic`).
    pub async fn invite_to_circle(
        &mut self,
        circle_id: SpaceId,
        member:    VerifyingKey,
        access:    Access<()>,
    ) -> Result<(), SyncError> {
        let topic = topic_for_circle(circle_id);
        let (auth_message, space_message) = zodia_circles::invite_to_circle(
            &self.circle_manager, &self.sync_store, circle_id, member, access,
        ).await?;
        self.broadcast_circle_op(topic, auth_message)?;
        self.broadcast_circle_op(topic, space_message)?;
        Ok(())
    }

    /// Revoke `member`'s access to `circle_id`. Key rotation for the
    /// remaining members happens automatically inside `p2panda-spaces` —
    /// see `docs/prd/circles.md`'s "Key rotation on revocation is automatic".
    pub async fn revoke_from_circle(
        &mut self,
        circle_id: SpaceId,
        member:    VerifyingKey,
    ) -> Result<(), SyncError> {
        let topic = topic_for_circle(circle_id);
        let (auth_message, space_message) = zodia_circles::revoke_from_circle(
            &self.circle_manager, &self.sync_store, circle_id, member,
        ).await?;
        self.broadcast_circle_op(topic, auth_message)?;
        self.broadcast_circle_op(topic, space_message)?;
        Ok(())
    }

    /// Current members of `circle_id` and their access levels.
    pub async fn circle_members(&self, circle_id: SpaceId) -> Result<Vec<(VerifyingKey, Access<()>)>, SyncError> {
        zodia_circles::circle_members(&self.circle_manager, circle_id).await.map_err(SyncError::from)
    }

    /// Encrypt and share `plaintext` (an encoded `InterpOp`/`DocOp`, same
    /// wire format as the public path — see `zodia_pipeline::materialize_circle_content`)
    /// to every current member of `circle_id`.
    pub async fn share_to_circle(&mut self, circle_id: SpaceId, plaintext: Vec<u8>) -> Result<(), SyncError> {
        let topic = topic_for_circle(circle_id);
        let message = zodia_circles::share_to_circle(
            &self.circle_manager, &self.sync_store, circle_id, &plaintext,
        ).await?;
        self.broadcast_circle_op(topic, message)?;
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
        self.open_topic(topic, log_id).await?;
        self.publish_bytes(op.encode(), log_id, topic).await
    }

    async fn publish_bytes(
        &mut self,
        payload_bytes: Vec<u8>,
        log_id:        u64,
        topic:         Topic,
    ) -> Result<(), SyncError> {
        // Determine the next sequence number + backlink from our log tip.
        // `get_latest_entry` (unlike `_tx`-suffixed store methods) doesn't
        // need an open transaction — `store_and_associate` below opens its
        // own for the insert+associate+commit that does.
        let latest: Option<Operation<OpExtensions>> = self.sync_store
            .get_latest_entry(&self.signing_key.verifying_key(), &log_id)
            .await
            .map_err(|e| SyncError::PandaStore(e.to_string()))?;

        let (seq_num, backlink) = match latest {
            Some(prev) => (prev.header.seq_num + 1, Some(prev.header.hash())),
            None       => (0, None),
        };

        let body_op = Body::new(&payload_bytes);

        let mut header = Header::<OpExtensions> {
            version:       1,
            verifying_key: self.signing_key.verifying_key(),
            signature:     None,
            payload_size:  body_op.size(),
            payload_hash:  Some(body_op.hash()),
            seq_num,
            backlink,
            extensions:    OpExtensions { timestamp: Timestamp::now() },
        };
        header.sign(&self.signing_key);
        let op_hash = header.hash();

        let operation = Operation {
            hash:   op_hash,
            header,
            body:   Some(body_op),
        };

        store_and_associate(
            &self.sync_store, topic, &self.signing_key.verifying_key(), log_id, &op_hash, &operation,
        ).await?;

        // `open_topic` (called by every publish path above) guarantees an
        // entry exists for `topic` by the time we get here.
        self.handles
            .get(&topic)
            .expect("publish_bytes called after open_topic")
            .publish(operation)
            .map_err(|e| SyncError::Sync(format!("{e:?}")))?;

        Ok(())
    }
}

/// Insert `operation` under `log_id` and associate `(topic, author, log_id)`
/// in one transaction — the store-write half both `publish_bytes` (for our
/// own authored ops) and the receive-path forwarder (for ops received from
/// someone else, see `open_topic`) need. `associate` is what lets a later
/// catch-up request — from a peer syncing with us, publisher or not — find
/// this `(author, log_id)` pair advertised on `topic`; without it the op
/// sits in `operations_v1` but is otherwise invisible to `TopicStore::resolve`
/// (see `docs/prd/granular-topic-subscription.md`'s "Bug found and fixed"
/// note — this is that same fix, generalised to non-self-authored ops).
///
/// Free function, not a method, so it's testable against a bare
/// `SqliteStore` without a live network, same reasoning as this crate's
/// other `associate`/transaction tests below.
async fn store_and_associate(
    store:     &SqliteStore,
    topic:     Topic,
    author:    &VerifyingKey,
    log_id:    u64,
    id:        &Hash,
    operation: &Operation<OpExtensions>,
) -> Result<(), SyncError> {
    let permit = store.begin().await.map_err(|e| SyncError::PandaStore(e.to_string()))?;

    if let Err(e) = store.insert_operation(id, operation, &log_id).await {
        let _ = store.rollback(permit).await;
        return Err(SyncError::PandaStore(e.to_string()));
    }
    if let Err(e) = store.associate(&topic, author, &log_id).await {
        let _ = store.rollback(permit).await;
        return Err(SyncError::PandaStore(e.to_string()));
    }

    store.commit(permit).await.map_err(|e| SyncError::PandaStore(e.to_string()))
}

/// Free function so it's testable against a bare `SqliteStore` without a
/// live network — same reasoning as this crate's `associate`/transaction
/// tests: `ZodiaSyncNode` itself needs a real `Endpoint`/`Gossip` to spawn.
///
/// p2panda-store 0.7 dropped the `operations_v1.timestamp` column (matching
/// `Header::timestamp`'s own removal upstream — see [`zodia_ops::OpExtensions`]).
/// The timestamp now only exists CBOR-encoded inside `operations_v1.header`,
/// so this reads the header blob back with the same `p2panda_core::cbor`
/// codec `p2panda-store` itself uses to write it, decodes each candidate's
/// `Timestamp` extension, and only then deletes the ones past cutoff — still
/// a single bulk `DELETE ... WHERE hash IN (...)` rather than one call per
/// row through `OperationStore::delete_operation`.
async fn prune_older_than(
    store:  &SqliteStore,
    keep:   &VerifyingKey,
    cutoff: Timestamp,
) -> Result<u64, SyncError> {
    let candidates: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT hash, header FROM operations_v1 WHERE verifying_key != ?",
    )
    .bind(keep.to_hex())
    .fetch_all(store.pool())
    .await
    .map_err(|e| SyncError::PandaStore(e.to_string()))?;

    let mut stale_hashes = Vec::new();
    for (hash_hex, header_bytes) in candidates {
        let header: Header<OpExtensions> = p2panda_core::cbor::decode_cbor(&header_bytes[..])
            .map_err(|e| SyncError::PandaStore(format!("failed to decode stored header: {e}")))?;
        let timestamp = header
            .extension::<Timestamp>()
            .expect("every zodia op carries a timestamp extension");
        if timestamp < cutoff {
            stale_hashes.push(hash_hex);
        }
    }

    if stale_hashes.is_empty() {
        return Ok(0);
    }

    let placeholders = std::iter::repeat("?").take(stale_hashes.len()).collect::<Vec<_>>().join(",");
    let query_str = format!("DELETE FROM operations_v1 WHERE hash IN ({placeholders})");
    let mut query = sqlx::query(&query_str);
    for hash_hex in &stale_hashes {
        query = query.bind(hash_hex);
    }
    let result = query
        .execute(store.pool())
        .await
        .map_err(|e| SyncError::PandaStore(e.to_string()))?;
    Ok(result.rows_affected())
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

    fn signed_op(key: &SigningKey, payload: &[u8]) -> Operation<OpExtensions> {
        signed_op_at(key, payload, Timestamp::now())
    }

    fn signed_op_at(key: &SigningKey, payload: &[u8], timestamp: Timestamp) -> Operation<OpExtensions> {
        let body = Body::new(payload);
        let mut header = Header::<OpExtensions> {
            version:       1,
            verifying_key: key.verifying_key(),
            signature:     None,
            payload_size:  body.size(),
            payload_hash:  Some(body.hash()),
            seq_num:       0,
            backlink:      None,
            extensions:    OpExtensions { timestamp },
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

    /// Regression test for a real gap found while building Phase D pruning:
    /// operations *received* from a peer were never persisted locally at
    /// all (only self-published ones were) — meaning this device could
    /// never re-serve them to a third peer, and pruning had nothing real
    /// to act on. `store_and_associate` is the fix: the same
    /// insert+associate+commit sequence `publish_bytes` already used for
    /// our own ops, generalised to store *anyone's* operation (the author
    /// here is deliberately not `self`'s identity, proving this isn't
    /// accidentally scoped to self-authored content).
    #[tokio::test]
    async fn store_and_associate_makes_a_received_operation_discoverable_by_a_third_peer() {
        // Given an operation authored by someone else entirely (not "me",
        // simulating what this device received from a peer during sync)
        let store  = SqliteStore::temporary().await;
        let author = SigningKey::generate();
        let topic  = Topic::from([9u8; 32]);
        let log_id: u64 = 77;
        let op = signed_op(&author, b"relayed content");

        // When storing and associating it as the receive path should
        store_and_associate(&store, topic, &author.verifying_key(), log_id, &op.hash, &op)
            .await.expect("store_and_associate");

        // Then a third peer's catch-up query against this device — which
        // is exactly what TopicStore::resolve backs — finds the original
        // author's log advertised on that topic, and the operation itself
        // is readable.
        let found: std::collections::BTreeMap<VerifyingKey, Vec<u64>> =
            store.resolve(&topic).await.expect("resolve");
        assert_eq!(found.get(&author.verifying_key()), Some(&vec![log_id]));

        let stored = OperationStore::<Operation<OpExtensions>, p2panda_core::Hash>::get_operation(&store, &op.hash)
            .await.expect("get");
        assert!(stored.is_some());
    }

    /// Old timestamp, well within the safe lexicographic-string-comparison
    /// range `prune_older_than` relies on (timestamps stay 16 decimal
    /// digits from year ~2001 to ~2287 — see that function's doc comment).
    fn old_timestamp() -> Timestamp {
        Timestamp::new(1_000_000_000_000_000)
    }

    /// `insert_operation` requires an already-open transaction (same
    /// constraint as `publish_bytes` — see `associate_after_commit_is_
    /// rejected...` above), so tests that just need an op sitting in the
    /// store wrap it here rather than repeating begin/commit each time.
    async fn insert_op(store: &SqliteStore, op: &Operation<OpExtensions>, log_id: u64) {
        let permit = store.begin().await.expect("begin");
        store.insert_operation(&op.hash, op, &log_id).await.expect("insert");
        store.commit(permit).await.expect("commit");
    }

    #[tokio::test]
    async fn pruning_keeps_own_authored_ops_regardless_of_age() {
        // Given an old op authored by "me"
        let store = SqliteStore::temporary().await;
        let me    = SigningKey::generate();
        let op    = signed_op_at(&me, b"my old contribution", old_timestamp());
        insert_op(&store, &op, 1).await;

        // When pruning everything older than "now"
        let removed = prune_older_than(&store, &me.verifying_key(), Timestamp::now())
            .await.expect("prune");

        // Then it's kept — 0 removed, and it's still readable.
        assert_eq!(removed, 0);
        let still_there = OperationStore::<Operation<OpExtensions>, p2panda_core::Hash>::get_operation(&store, &op.hash)
            .await.expect("get");
        assert!(still_there.is_some());
    }

    #[tokio::test]
    async fn pruning_removes_old_ops_from_other_authors_but_keeps_recent_ones() {
        // Given an old op and a recent op, both from someone else
        let store = SqliteStore::temporary().await;
        let me    = SigningKey::generate();
        let other = SigningKey::generate();
        let old_op    = signed_op_at(&other, b"their old contribution", old_timestamp());
        let recent_op = signed_op_at(&other, b"their recent contribution", Timestamp::now());
        insert_op(&store, &old_op, 1).await;
        insert_op(&store, &recent_op, 2).await;

        // When pruning everything older than just-after the old timestamp
        let cutoff = Timestamp::new(u64::from(old_timestamp()) + 1);
        let removed = prune_older_than(&store, &me.verifying_key(), cutoff)
            .await.expect("prune");

        // Then only the old op is gone; the recent one remains.
        assert_eq!(removed, 1);
        let old_still_there = OperationStore::<Operation<OpExtensions>, p2panda_core::Hash>::get_operation(&store, &old_op.hash)
            .await.expect("get old");
        let recent_still_there = OperationStore::<Operation<OpExtensions>, p2panda_core::Hash>::get_operation(&store, &recent_op.hash)
            .await.expect("get recent");
        assert!(old_still_there.is_none());
        assert!(recent_still_there.is_some());
    }
}
