//! Per-key collaborative document with author-veto ring.
//!
//! Wraps `loro` (text CRDT) and tracks the last `RING_SIZE` editors of each
//! block.  Phase F-collab tracer uses single-block semantics (the whole
//! doc body is one block); multi-block decomposition lands in the iteration
//! after this — the public API leaves the door open.
//!
//! # Types
//!
//! - [`InterpDoc`] — handle to one key's editable text.
//! - [`Edit`] — output of [`InterpDoc::publish_local`]: the bytes to ship
//!   on the wire plus the set of `block_id`s the edit touched.
//! - [`Ring`] — fixed-capacity FIFO of recent block authors for veto auth.
//!
//! # Threading
//!
//! `LoroDoc` is `!Send`; callers drive `InterpDoc` from a single thread.
//! Zodia uses the relm4 main-loop tokio runtime + `LocalSet` already.

use std::collections::VecDeque;

pub use ed25519_dalek::VerifyingKey;
use loro::{ExportMode, LoroDoc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of recent block authors retained per block.  A veto from
/// any of these authors within `VETO_WINDOW_DAYS` rolls back their target.
pub const RING_SIZE: usize = 5;

/// Days after an edit was published during which a ring author may veto it.
pub const VETO_WINDOW_DAYS: u64 = 7;

/// Block id.  Phase F-collab tracer: the single canonical block is the
/// all-zeros id.  Multi-block decomposition will use random ids from
/// `getrandom`.
pub type BlockId = [u8; 16];

/// The single-block id used for the entire doc body in tracer mode.
pub const BODY_BLOCK_ID: BlockId = [0u8; 16];

/// Loro container name we store the doc body under inside the LoroDoc root.
const BODY_KEY: &str = "body";

// ── errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum DocError {
    #[error("loro: {0}")]
    Loro(#[from] loro::LoroError),
    #[error("loro encode: {0}")]
    LoroEncode(#[from] loro::LoroEncodeError),
    #[error("missing body container")]
    NoBody,
}

// ── document handle ──────────────────────────────────────────────────────────

/// One collaboratively edited document, scoped to a single `interp_key`.
///
/// Apply remote edits via [`apply_remote`].  Make local edits to
/// [`body_mut`] then call [`publish_local`] to produce the wire-format
/// update bytes + the set of touched block ids.
pub struct InterpDoc {
    inner: LoroDoc,
    /// Pubkey we attribute local edits to.  Stamped onto Loro's
    /// `set_peer_id` to keep CRDT log attribution legible.
    local_peer: u64,
}

impl InterpDoc {
    /// Open a fresh, empty document.  Use [`from_snapshot`] to restore
    /// from persisted bytes instead.
    pub fn empty(local: &VerifyingKey) -> Self {
        let inner = LoroDoc::new();
        let local_peer = peer_id_from(local);
        let _ = inner.set_peer_id(local_peer);
        Self { inner, local_peer }
    }

    /// Restore from a previously [`snapshot()`]-ed byte blob.
    pub fn from_snapshot(local: &VerifyingKey, bytes: &[u8]) -> Result<Self, DocError> {
        let inner = LoroDoc::new();
        let local_peer = peer_id_from(local);
        let _ = inner.set_peer_id(local_peer);
        inner.import(bytes)?;
        Ok(Self { inner, local_peer })
    }

    /// Snapshot the doc for persistence.  Captures the full op log; size
    /// grows over time until Phase K pruning lands.
    pub fn snapshot(&self) -> Result<Vec<u8>, DocError> {
        Ok(self.inner.export(ExportMode::Snapshot)?)
    }

    /// Current full text of the doc body.
    pub fn body_text(&self) -> String {
        self.inner.get_text(BODY_KEY).to_string()
    }

    /// Replace the body's text with `new`.  Use this for local edits.  Call
    /// [`publish_local`] after a batch of edits to emit the wire-format
    /// update for sync.
    pub fn set_body(&self, new: &str) -> Result<(), DocError> {
        let text = self.inner.get_text(BODY_KEY);
        let len = text.len_unicode();
        if len > 0 {
            text.delete(0, len)?;
        }
        if !new.is_empty() {
            text.insert(0, new)?;
        }
        Ok(())
    }

    /// Apply a remote update emitted by another node's [`publish_local`].
    /// Returns the set of block ids whose ring should be updated to include
    /// the remote editor.
    pub fn apply_remote(&self, update: &[u8]) -> Result<Vec<BlockId>, DocError> {
        self.inner.import(update)?;
        // Single-block tracer: any edit touches the whole-body block.
        Ok(vec![BODY_BLOCK_ID])
    }

    /// Encode all locally-committed but un-shipped changes as an update
    /// blob.  The returned [`Edit`] carries the wire bytes and the block
    /// ids touched (always `[BODY_BLOCK_ID]` in tracer mode).
    pub fn publish_local(&self) -> Result<Edit, DocError> {
        self.inner.commit();
        let update = self.inner.export(ExportMode::all_updates())?;
        Ok(Edit {
            update_bytes:    update,
            affected_blocks: vec![BODY_BLOCK_ID],
            rev:             self.current_rev(),
        })
    }

    /// Stable revision id of the current doc state.  Used by `DocOp::Edit`
    /// to record the `base_rev` an edit was made against, and by
    /// `DocOp::AffirmRev` to target a specific revision.
    pub fn current_rev(&self) -> [u8; 32] {
        // Loro's `state_frontiers()` returns a Vec<ID>; hash to a stable
        // 32-byte digest so callers can use it as an op field.
        let frontiers = self.inner.state_frontiers();
        let mut hasher = blake3::Hasher::new();
        for id in frontiers.iter() {
            hasher.update(&id.peer.to_le_bytes());
            hasher.update(&id.counter.to_le_bytes());
        }
        *hasher.finalize().as_bytes()
    }

    /// Local peer id (derived from the local identity).
    pub fn local_peer_id(&self) -> u64 {
        self.local_peer
    }
}

/// Output of [`InterpDoc::publish_local`] — the bytes to put inside a
/// `DocOp::Edit` and the blocks the edit affects (for ring updates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edit {
    pub update_bytes:    Vec<u8>,
    pub affected_blocks: Vec<BlockId>,
    /// The doc revision *after* applying the edit.
    pub rev:             [u8; 32],
}

// ── author ring ──────────────────────────────────────────────────────────────

/// Fixed-capacity FIFO of recent block authors.  Newest at the back; oldest
/// falls off the front when capacity is reached.  Lookup is linear (N=5 →
/// negligible).  Persisted to `doc_block_authors` by the store layer.
#[derive(Debug, Clone, Default)]
pub struct Ring {
    entries: VecDeque<RingEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingEntry {
    pub author:     [u8; 32],
    pub edit_op_id: [u8; 32],
    pub edited_at:  u64,
}

impl Ring {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries(entries: Vec<RingEntry>) -> Self {
        let mut deque: VecDeque<RingEntry> = entries.into_iter().collect();
        while deque.len() > RING_SIZE {
            deque.pop_front();
        }
        Self { entries: deque }
    }

    /// Push a new edit author onto the ring; pop the oldest if at capacity.
    pub fn push(&mut self, entry: RingEntry) {
        self.entries.push_back(entry);
        while self.entries.len() > RING_SIZE {
            self.entries.pop_front();
        }
    }

    /// True iff `author` currently sits in the ring.
    pub fn contains(&self, author: &[u8; 32]) -> bool {
        self.entries.iter().any(|e| &e.author == author)
    }

    /// Newest entry, if any.  Used by veto authority checks: "did the
    /// veto target the most recent edit on this block?"
    pub fn newest(&self) -> Option<&RingEntry> {
        self.entries.back()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RingEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ── veto authority ────────────────────────────────────────────────────────────

/// True iff `revoker` is allowed to veto an edit identified by
/// `target_edit_op_id`, given the block's current `ring` state and the
/// edit's `edit_unix_ts`.  The revoker must:
///   1. sit in the ring, AND
///   2. issue the veto within `VETO_WINDOW_DAYS` of the edit, AND
///   3. target the *most recent* edit on the block (later edits invalidate
///      stale vetoes).
pub fn veto_authorised(
    ring:              &Ring,
    revoker:           &[u8; 32],
    target_edit_op_id: &[u8; 32],
    edit_unix_ts:      u64,
    now_unix_ts:       u64,
) -> bool {
    if !ring.contains(revoker) {
        return false;
    }
    let window_secs = VETO_WINDOW_DAYS * 86_400;
    if now_unix_ts.saturating_sub(edit_unix_ts) > window_secs {
        return false;
    }
    match ring.newest() {
        Some(entry) => &entry.edit_op_id == target_edit_op_id,
        None        => false,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Derive a Loro peer id (u64) from a verifying key.  Loro uses the peer
/// id to attribute ops in its internal log; using a stable per-identity
/// value makes the log inspectable.  We take the first 8 bytes of the
/// pubkey — collisions across 2^32 peers are negligibly likely.
fn peer_id_from(vk: &VerifyingKey) -> u64 {
    let bytes = vk.as_bytes();
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(id)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn signing_key(seed: u8) -> SigningKey {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        SigningKey::from_bytes(&bytes)
    }

    #[test]
    fn empty_then_set_then_snapshot_then_restore() {
        let sk = signing_key(1);
        let vk = sk.verifying_key();
        let doc = InterpDoc::empty(&vk);
        doc.set_body("Hello, community.").unwrap();
        doc.publish_local().unwrap();
        assert_eq!(doc.body_text(), "Hello, community.");

        let snapshot = doc.snapshot().unwrap();
        let restored = InterpDoc::from_snapshot(&vk, &snapshot).unwrap();
        assert_eq!(restored.body_text(), "Hello, community.");
    }

    #[test]
    fn two_peers_converge_via_apply_remote() {
        let sk_a = signing_key(1);
        let sk_b = signing_key(2);
        let doc_a = InterpDoc::empty(&sk_a.verifying_key());
        let doc_b = InterpDoc::empty(&sk_b.verifying_key());

        doc_a.set_body("First draft.").unwrap();
        let edit_a = doc_a.publish_local().unwrap();
        doc_b.apply_remote(&edit_a.update_bytes).unwrap();
        assert_eq!(doc_b.body_text(), "First draft.");
    }

    #[test]
    fn ring_evicts_oldest() {
        let mut ring = Ring::new();
        for i in 0..(RING_SIZE as u8 + 2) {
            let mut author = [0u8; 32];
            author[0] = i;
            ring.push(RingEntry {
                author,
                edit_op_id: [i; 32],
                edited_at:  i as u64,
            });
        }
        assert_eq!(ring.len(), RING_SIZE);
        // Oldest (i=0,1) evicted.
        let first_author = ring.iter().next().unwrap().author;
        assert_eq!(first_author[0], 2);
    }

    #[test]
    fn veto_authorised_only_for_ring_within_window_targeting_newest() {
        let mut ring = Ring::new();
        let alice = [1u8; 32];
        let bob   = [2u8; 32];
        let alice_edit = [10u8; 32];
        ring.push(RingEntry { author: alice, edit_op_id: alice_edit, edited_at: 1000 });

        // Bob is not in the ring → cannot veto.
        assert!(!veto_authorised(&ring, &bob, &alice_edit, 1000, 2000));

        // Alice within window, targets newest → ok.
        assert!(veto_authorised(&ring, &alice, &alice_edit, 1000, 2000));

        // Alice past window → no.
        let past_window = 1000 + (VETO_WINDOW_DAYS * 86_400) + 1;
        assert!(!veto_authorised(&ring, &alice, &alice_edit, 1000, past_window));

        // Alice targets stale edit (newer edit landed) → no.
        let bob_edit = [11u8; 32];
        ring.push(RingEntry { author: bob, edit_op_id: bob_edit, edited_at: 1500 });
        assert!(!veto_authorised(&ring, &alice, &alice_edit, 1000, 2000));
    }
}
