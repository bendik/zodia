//! SQLite persistence for Zodia via p2panda-store.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE interpretations (
//!     log_id      BLOB NOT NULL PRIMARY KEY,
//!     interp_key  TEXT NOT NULL,   -- InterpKey::to_sig(), e.g. "transit:saturn_square_natal_venus"
//!     interp_kind TEXT NOT NULL,   -- "natal" | "synastry" | "transit" | "house_transit"
//!     body        TEXT NOT NULL,
//!     author_pk   BLOB,            -- nullable: anonymous contributions allowed
//!     received_at INTEGER NOT NULL
//! );
//! CREATE INDEX idx_interp_key  ON interpretations(interp_key);
//! CREATE INDEX idx_interp_kind ON interpretations(interp_kind);
//!
//! CREATE TABLE affirmations (
//!     log_id        BLOB NOT NULL PRIMARY KEY,
//!     interp_log_id BLOB NOT NULL,
//!     author_pk     BLOB NOT NULL,  -- non-nullable: affirmations are pseudonymous, not anonymous
//!     created_at    INTEGER NOT NULL,
//!     UNIQUE(interp_log_id, author_pk)  -- one affirmation per peer per interpretation
//! );
//! CREATE INDEX idx_affirmation_interp ON affirmations(interp_log_id);
//! ```
//!
//! The `p2panda-store` dependency and SQLite wiring will be added when crate
//! versions are pinned.

use serde::{Deserialize, Serialize};
use zodia_core::InterpKey;

/// A community-contributed interpretation of an astrological phenomenon.
///
/// Each interpretation is a signed p2panda operation that replicates freely
/// across the swarm.  Authorship is pseudonymous — `author_pk` is the peer's
/// long-term p2panda pubkey, not tied to any real-world identity.
///
/// The `interp_key` determines *what* is being interpreted:
///   - Natal aspect     → `"natal:jupiter_trine_venus"`
///   - Synastry aspect  → `"synastry:jupiter_trine_venus"`
///   - Transit aspect   → `"transit:saturn_square_natal_venus"`
///   - House transit    → `"house_transit:saturn:7"`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interpretation {
    pub key: InterpKey,
    pub body: String,
    /// `None` = anonymous contribution; author chose not to sign with their identity.
    pub author_pk: Option<[u8; 32]>,
    /// p2panda operation hash — stable identifier for this entry.
    pub log_id: [u8; 32],
    pub received_at: u64,
}

/// A peer's affirmation of an interpretation.
///
/// Affirmations are lightweight signed p2panda operations — a "this resonated
/// with me" signal that surfaces high-signal contributions without ranking
/// algorithms or centralised votes.
///
/// Properties:
///   - **Not anonymous** — the `author_pk` prevents sybil inflation.
///   - **One per peer per interpretation** — enforced by a DB UNIQUE constraint.
///   - **No body text** — affirmations are pure signal, not discussion.
///   - **Freely replicating** — peers who sync catch up on affirmation counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Affirmation {
    /// The `log_id` of the interpretation being affirmed.
    pub interp_log_id: [u8; 32],
    /// Author's p2panda pubkey — pseudonymous but Sybil-resistant.
    pub author_pk: [u8; 32],
    /// This affirmation's own p2panda operation hash.
    pub log_id: [u8; 32],
    pub created_at: u64,
}
