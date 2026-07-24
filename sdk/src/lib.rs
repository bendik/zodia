//! Relm4-agnostic client facade over `zodia-net` + `zodia-sync` +
//! `zodia-pipeline` — see `docs/prd/zodia-sdk.md`.
//!
//! `p2panda-stream` (used inside `zodia-pipeline`) is `!Send`, so the whole
//! network/sync/pipeline stack lives on one dedicated OS thread running a
//! single-threaded tokio runtime + `LocalSet`. Everything [`ZodiaClient`]
//! exposes outward — the command methods and the [`StateEvent`] stream —
//! crosses that boundary through ordinary `Send` channels, so the caller
//! can be any async runtime (or none): relm4/glib, plain tokio, a test.

use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;

use ed25519_dalek::SigningKey;
use p2panda_core::{Hash, SigningKey as PandaSigningKey, Topic};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::warn;

use zodia_core::{BirthData, topic_key_global};
use zodia_net::{NetworkConfig, PeerId, ZodiaNetwork};
use zodia_ops::{DocOp, InterpOp};
use zodia_pipeline::ZodiaPipeline;
use zodia_sync::{SyncEvent, SyncError, ZodiaSyncNode};

pub use zodia_pipeline::StateEvent;

// ── public config / errors ──────────────────────────────────────────────────

/// Everything needed to bring a `ZodiaClient` up.
pub struct ZodiaClientConfig {
    pub signing_key: SigningKey,
    pub birth:       BirthData,
    pub data_dir:    PathBuf,
}

#[derive(Debug, Error)]
pub enum ClientError {
    /// The client's background thread is gone — either it never started
    /// successfully, or the `ZodiaClient` handle was dropped.
    #[error("client thread is gone")]
    Disconnected,
    #[error("network: {0}")]
    Network(String),
    #[error("sync: {0}")]
    Sync(String),
}

/// Catch-up state, derived from `SyncStarted`/`SyncFinished`/`Failed`
/// lifecycle events per peer. Snapshot, not a log — only the latest state
/// matters to a UI status panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStatus {
    pub peers_known:     usize,
    pub peers_caught_up: usize,
}

// ── client handle ────────────────────────────────────────────────────────────

/// `Send + 'static` handle to a running Zodia network/sync/pipeline stack.
/// Cloneable-in-spirit via `events()`/`sync_status()` (each call gets its
/// own receiver); the command methods go through one shared channel to the
/// background thread.
pub struct ZodiaClient {
    cmd_tx:     mpsc::Sender<Command>,
    events_tx:  broadcast::Sender<StateEvent>,
    status_rx:  watch::Receiver<SyncStatus>,
    node_id:    PeerId,
    #[cfg(test)]
    thread: Option<thread::JoinHandle<()>>,
}

impl ZodiaClient {
    /// Spawn the dedicated thread, bring up network + sync + pipeline, and
    /// return once the endpoint is live. Callable from any async runtime.
    pub async fn connect(config: ZodiaClientConfig) -> Result<Self, ClientError> {
        let (ready_tx, ready_rx) = oneshot::channel::<Result<ConnectReady, ClientError>>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
        let (events_tx, _events_rx) = broadcast::channel::<StateEvent>(256);
        let (status_tx, status_rx) = watch::channel(SyncStatus::default());

        let events_tx_bg = events_tx.clone();
        let handle = thread::Builder::new()
            .name("zodia-sdk".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("zodia-sdk: build current-thread runtime");
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, run(config, cmd_rx, events_tx_bg, status_tx, ready_tx));
            })
            .expect("zodia-sdk: spawn dedicated thread");

        // Outside tests nothing joins this thread — it runs until `cmd_tx`
        // (held by `Self`, below) drops, then exits on its own.
        #[cfg(not(test))]
        let _ = &handle;

        let ready = ready_rx.await.map_err(|_| ClientError::Disconnected)?;
        let ConnectReady { node_id } = ready?;

        Ok(Self {
            cmd_tx,
            events_tx,
            status_rx,
            node_id,
            #[cfg(test)]
            thread: Some(handle),
        })
    }

    /// Every materialised `StateEvent`, network-wide. `broadcast` so
    /// multiple listeners (feed view, bell badge, an open aspect page)
    /// can each hold their own receiver off one internal stream. A slow
    /// listener gets `RecvError::Lagged(n)`, not a stall of everyone else.
    pub fn events(&self) -> broadcast::Receiver<StateEvent> {
        self.events_tx.subscribe()
    }

    /// Latest catch-up snapshot. `watch`, not a log — only the current
    /// state matters for a status panel.
    pub fn sync_status(&self) -> watch::Receiver<SyncStatus> {
        self.status_rx.clone()
    }

    pub fn node_id(&self) -> PeerId {
        self.node_id.clone()
    }

    /// Open a key's per-key sync topic (Phase C-2). Idempotent.
    pub async fn subscribe(&self, interp_key: &str) -> Result<(), ClientError> {
        self.call(|reply| Command::Subscribe { interp_key: interp_key.to_string(), reply }).await
    }

    /// Close a key's per-key sync topic. Idempotent.
    pub async fn unsubscribe(&self, interp_key: &str) -> Result<(), ClientError> {
        self.call(|reply| Command::Unsubscribe { interp_key: interp_key.to_string(), reply }).await
    }

    /// Legacy whole-interpretation authoring (`InterpOp::Author`, log 0).
    pub async fn author(&self, interp_key: &str, body: String) -> Result<(), ClientError> {
        self.call(|reply| Command::Author { interp_key: interp_key.to_string(), body, reply }).await
    }

    /// Apply a CRDT edit to a key's collaborative doc.
    pub async fn edit(
        &self,
        interp_key:      &str,
        base_rev:        Hash,
        crdt_update:     Vec<u8>,
        affected_blocks: Vec<[u8; 16]>,
    ) -> Result<(), ClientError> {
        self.call(|reply| Command::Edit {
            interp_key: interp_key.to_string(), base_rev, crdt_update, affected_blocks, reply,
        }).await
    }

    /// Veto a specific edit, within the author-ring's window.
    pub async fn veto(&self, interp_key: &str, target_edit_op_id: Hash) -> Result<(), ClientError> {
        self.call(|reply| Command::Veto {
            interp_key: interp_key.to_string(), target_edit_op_id, reply,
        }).await
    }

    /// Affirm a specific revision of a key's doc.
    pub async fn affirm_rev(&self, interp_key: &str, target_rev: [u8; 32]) -> Result<(), ClientError> {
        self.call(|reply| Command::AffirmRev {
            interp_key: interp_key.to_string(), target_rev, reply,
        }).await
    }

    /// Presence heartbeat for a key's editor session.
    pub async fn set_editor_presence(&self, interp_key: &str, joined: bool) -> Result<(), ClientError> {
        self.call(|reply| Command::SetEditorPresence {
            interp_key: interp_key.to_string(), joined, reply,
        }).await
    }

    /// Send `build(reply_tx)` to the background thread and await its reply.
    /// Every command method above is this, mechanically — see
    /// `docs/prd/zodia-sdk.md`'s note on why `try_send`-and-forget (today's
    /// `SyncPublishMsg` pattern) doesn't carry over: callers here get a
    /// real `Result`.
    async fn call(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<(), ClientError>>) -> Command,
    ) -> Result<(), ClientError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx.send(build(reply_tx)).await.map_err(|_| ClientError::Disconnected)?;
        reply_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Test-only: hand back the background thread's `JoinHandle` so a test
    /// can drop the client and then `.join()` to prove the thread actually
    /// exits rather than leaking. Not part of the public API.
    #[cfg(test)]
    fn take_thread_handle(&mut self) -> thread::JoinHandle<()> {
        self.thread.take().expect("thread handle already taken")
    }
}

// ── background thread ────────────────────────────────────────────────────────

struct ConnectReady {
    node_id: PeerId,
}

enum Command {
    Author { interp_key: String, body: String, reply: Reply },
    Edit {
        interp_key:      String,
        base_rev:        Hash,
        crdt_update:     Vec<u8>,
        affected_blocks: Vec<[u8; 16]>,
        reply:           Reply,
    },
    Veto { interp_key: String, target_edit_op_id: Hash, reply: Reply },
    AffirmRev { interp_key: String, target_rev: [u8; 32], reply: Reply },
    SetEditorPresence { interp_key: String, joined: bool, reply: Reply },
    Subscribe { interp_key: String, reply: Reply },
    Unsubscribe { interp_key: String, reply: Reply },
}

type Reply = oneshot::Sender<Result<(), ClientError>>;

/// Runs for the lifetime of the client, on the dedicated thread's
/// `LocalSet`. Brings up network + sync + pipeline, reports readiness via
/// `ready_tx`, then pumps commands in and `StateEvent`s out until `cmd_rx`
/// closes (i.e. the `ZodiaClient` was dropped).
async fn run(
    config:    ZodiaClientConfig,
    mut cmd_rx: mpsc::Receiver<Command>,
    events_tx: broadcast::Sender<StateEvent>,
    status_tx: watch::Sender<SyncStatus>,
    ready_tx:  oneshot::Sender<Result<ConnectReady, ClientError>>,
) {
    let net_config = NetworkConfig { signing_key: config.signing_key.clone() };
    let (net, mut net_events) = match ZodiaNetwork::spawn(net_config, &config.birth).await {
        Ok(pair) => pair,
        Err(e) => { let _ = ready_tx.send(Err(ClientError::Network(e.to_string()))); return; }
    };
    let _ = net.publish_announce().await;

    let panda_key = PandaSigningKey::from_bytes(config.signing_key.as_bytes());
    let sync_topic = Topic::from(topic_key_global().0);
    let mut node = match ZodiaSyncNode::spawn(
        panda_key, net.endpoint(), net.gossip(), sync_topic, &config.data_dir,
    ).await {
        Ok(n) => n,
        Err(e) => { let _ = ready_tx.send(Err(ClientError::Sync(e.to_string()))); return; }
    };

    let node_id = net.node_id();
    if ready_tx.send(Ok(ConnectReady { node_id })).is_err() {
        // Caller dropped the `connect()` future (or the whole client)
        // before we finished — nobody left to serve, exit quietly.
        return;
    }

    // v1 doesn't act on peer-discovery/consent events, but the sender side
    // (`spawn_gossip_listener` et al) does `.send().await` into this
    // channel — an undrained receiver would eventually stall those tasks.
    tokio::task::spawn_local(async move {
        while net_events.recv().await.is_some() {}
    });

    let pipeline = ZodiaPipeline::new();
    let mut peer_caught_up: HashMap<[u8; 32], bool> = HashMap::new();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => handle_command(&mut node, cmd).await,
                    None      => break, // ZodiaClient dropped — tear down.
                }
            }
            Some(sync_event) = node.inbound.recv() => {
                match sync_event {
                    SyncEvent::OperationReceived(op) => {
                        if pipeline.process(*op).await.is_err() {
                            warn!("zodia-sdk: pipeline closed unexpectedly");
                            break;
                        }
                        match pipeline.next().await {
                            Ok(event) => { let _ = events_tx.send(event); }
                            Err(e)    => warn!("zodia-sdk: pipeline next: {e}"),
                        }
                    }
                    SyncEvent::SyncStarted { remote } => {
                        peer_caught_up.insert(*remote.as_bytes(), false);
                        let _ = status_tx.send(status_from(&peer_caught_up));
                    }
                    SyncEvent::SyncFinished { remote, .. } => {
                        peer_caught_up.insert(*remote.as_bytes(), true);
                        let _ = status_tx.send(status_from(&peer_caught_up));
                    }
                    SyncEvent::Failed { remote, error } => {
                        warn!("zodia-sdk: sync failed: {error}");
                        peer_caught_up.insert(*remote.as_bytes(), false);
                        let _ = status_tx.send(status_from(&peer_caught_up));
                    }
                }
            }
            else => break,
        }
    }
}

fn status_from(peer_caught_up: &HashMap<[u8; 32], bool>) -> SyncStatus {
    SyncStatus {
        peers_known:     peer_caught_up.len(),
        peers_caught_up: peer_caught_up.values().filter(|&&caught_up| caught_up).count(),
    }
}

async fn handle_command(node: &mut ZodiaSyncNode, cmd: Command) {
    match cmd {
        Command::Author { interp_key, body, reply } => {
            let res = node.publish(InterpOp::Author { interp_key, body }).await.map_err(sync_err);
            let _ = reply.send(res);
        }
        Command::Edit { interp_key, base_rev, crdt_update, affected_blocks, reply } => {
            let op = DocOp::Edit { interp_key, base_rev, crdt_update, affected_blocks };
            let _ = reply.send(node.publish_doc(op).await.map_err(sync_err));
        }
        Command::Veto { interp_key, target_edit_op_id, reply } => {
            let op = DocOp::Veto { interp_key, target_edit_op_id };
            let _ = reply.send(node.publish_doc(op).await.map_err(sync_err));
        }
        Command::AffirmRev { interp_key, target_rev, reply } => {
            let op = DocOp::AffirmRev { interp_key, target_rev };
            let _ = reply.send(node.publish_doc(op).await.map_err(sync_err));
        }
        Command::SetEditorPresence { interp_key, joined, reply } => {
            let op = DocOp::EditorPresence { interp_key, joined };
            let _ = reply.send(node.publish_doc(op).await.map_err(sync_err));
        }
        Command::Subscribe { interp_key, reply } => {
            let _ = reply.send(node.subscribe(&interp_key).await.map_err(sync_err));
        }
        Command::Unsubscribe { interp_key, reply } => {
            node.unsubscribe(&interp_key);
            let _ = reply.send(Ok(()));
        }
    }
}

fn sync_err(e: SyncError) -> ClientError {
    ClientError::Sync(e.to_string())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use rand_core::OsRng;
    use tokio::time::timeout;

    fn test_config(tmp: &tempdir_shim::TempDir, seed: u8) -> ZodiaClientConfig {
        let _ = seed;
        ZodiaClientConfig {
            signing_key: SigningKey::generate(&mut OsRng),
            birth:       zodia_core::birth_from_coords(2_451_545.0, 59.9, 10.7, 9),
            data_dir:    tmp.path().to_path_buf(),
        }
    }

    #[tokio::test]
    async fn connect_reports_a_stable_node_id() {
        let tmp = tempdir_shim::TempDir::new();
        let client = ZodiaClient::connect(test_config(&tmp, 1)).await
            .expect("connect");
        let id_a = client.node_id();
        let id_b = client.node_id();
        assert_eq!(id_a, id_b);
    }

    #[tokio::test]
    async fn dropping_the_client_tears_down_its_thread() {
        let tmp = tempdir_shim::TempDir::new();
        let mut client = ZodiaClient::connect(test_config(&tmp, 2)).await
            .expect("connect");
        let handle = client.take_thread_handle();
        drop(client);
        // Joining must return promptly — a leaked thread would hang here.
        tokio::task::spawn_blocking(move || handle.join())
            .await
            .expect("join task panicked")
            .expect("zodia-sdk thread panicked");
    }

    #[tokio::test]
    async fn subscribe_publish_receive_round_trip() {
        let tmp_a = tempdir_shim::TempDir::new();
        let tmp_b = tempdir_shim::TempDir::new();
        let a = ZodiaClient::connect(test_config(&tmp_a, 3)).await.expect("connect a");
        let b = ZodiaClient::connect(test_config(&tmp_b, 4)).await.expect("connect b");

        let key = "natal:sdk_test_round_trip";
        a.subscribe(key).await.expect("a subscribe");
        b.subscribe(key).await.expect("b subscribe");

        let mut a_events = a.events();

        // Give discovery a moment before publishing — mirrors the existing
        // net/tests/channel.rs pattern of a short settle window.
        tokio::time::sleep(Duration::from_millis(500)).await;

        b.edit(key, Hash::from_bytes([0u8; 32]), vec![1, 2, 3], vec![[9u8; 16]]).await
            .expect("b edit");

        let event = timeout(Duration::from_secs(15), async {
            loop {
                match a_events.recv().await.expect("events channel closed") {
                    StateEvent::DocEdited { interp_key, .. } if interp_key == key => return true,
                    _ => continue,
                }
            }
        }).await;

        assert!(event.is_ok(), "a did not observe b's edit within 15s");
    }

    /// Minimal stand-in for `tempfile::TempDir` — avoids adding a new
    /// workspace dependency for two dev-only tests. Cleans up on drop.
    mod tempdir_shim {
        use std::path::{Path, PathBuf};

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                let mut p = std::env::temp_dir();
                let unique = format!(
                    "zodia-sdk-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                );
                p.push(unique);
                std::fs::create_dir_all(&p).expect("create temp dir");
                Self(p)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
