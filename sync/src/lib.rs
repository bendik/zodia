//! Stub sync layer — temporarily inert during the p2panda 0.6 migration.
//!
//! The 0.6 LogSync API moved enough that `ZodiaSyncNode` cannot be ported as
//! a one-shot rename — the in-memory `MemoryStore` is gone, `TopicMap` moved,
//! and the `Header` struct shape changed.  Rather than block the rest of the
//! migration on a sync-layer rewrite, this file exposes the same public
//! surface as before but performs no actual sync.  The user-facing impact:
//! offline catch-up of community interpretations is disabled until Phase 3
//! lands; live Tier-1 direct exchange (via `ConsentChannel` in zodia-net)
//! is unaffected.
//!
//! Restore real behaviour by reimplementing against `p2panda_net::sync::LogSync`
//! using `p2panda_store::SqliteStore` as the backing store.

use p2panda_core::{SigningKey, Topic};
use p2panda_net::{Endpoint, Gossip};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::warn;

use zodia_store::{StoreError, ZodiaStore};

// ── error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("p2panda sync: {0}")]
    Sync(String),
    #[error("payload encode/decode failed")]
    Payload,
}

// ── received interpretation ──────────────────────────────────────────────────

/// A decoded interpretation that arrived via LogSync from a remote peer.
///
/// Currently unreachable while the stub is in place; kept on the public API
/// so `app::AppMsg::SyncInterpReceived` still type-checks.
#[derive(Debug, Clone)]
pub struct ReceivedInterp {
    pub interp_key: String,
    pub body:       String,
    pub author_pk:  [u8; 32],
    pub author_sig: [u8; 64],
}

// ── sync node ─────────────────────────────────────────────────────────────────

/// Inert sync handle.  `received` will never produce values until Phase 3
/// reinstates the real LogSync implementation.
pub struct ZodiaSyncNode {
    pub received: mpsc::Receiver<ReceivedInterp>,
}

impl ZodiaSyncNode {
    /// Pretend to spawn the sync node.  Logs a one-time warning and returns an
    /// inert handle whose `received` channel never fires.
    pub async fn spawn(
        _signing_key: SigningKey,
        _endpoint:    Endpoint,
        _gossip:      Gossip,
        _store:       ZodiaStore,
        _sync_topic:  Topic,
    ) -> Result<Self, SyncError> {
        warn!("zodia-sync is stubbed during the p2panda 0.6 migration — \
               offline catch-up of community interpretations is disabled");
        let (_tx, rx) = mpsc::channel(1);
        Ok(Self { received: rx })
    }

    /// Pretend to publish.  No-op until Phase 3.
    pub async fn publish(
        &mut self,
        _interp_key: &str,
        _body:       &str,
        _author_sig: &[u8; 64],
    ) -> Result<(), SyncError> {
        Ok(())
    }
}
