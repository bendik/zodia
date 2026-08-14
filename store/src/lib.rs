//! SQLite-backed store for aspect interpretations and affirmations.
//!
//! # Design notes
//!
//! - Interpretations are keyed by `InterpKey::to_sig()` — a stable canonical string.
//! - Baseline entries (seeded from the bundled TOML) carry `is_baseline = 1`; they
//!   lose to any community-contributed entry in the ranking, even ones with zero
//!   affirmations.  This ensures the app feels alive even on first run while never
//!   crowding out genuine peer contributions.
//! - Affirmations are unique per (interp_log_id, author_pk) — sybil-resistant without
//!   requiring identity disclosure beyond a pseudonymous pubkey.
//! - Community entries carry an ed25519 `author_sig` over `log_id = BLAKE3(key||body)`.
//!   `insert_received` verifies the signature before writing, making the store the
//!   final gatekeeper against forged peer contributions.
//!
//! All public methods are `async` and run on the calling tokio runtime.  The
//! underlying `SqlitePool` is cheap to clone and `Send + Sync`, so the store
//! can be shared across tasks without external locking.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, VerifyingKey};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use zodia_core::InterpKey;

pub use seed::{BaselineData, BaselineStore};

pub mod seed;

// ── error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("seed parse error: {0}")]
    Seed(String),
    #[error("invalid interpretation signature")]
    InvalidSignature,
}

// ── community entry ───────────────────────────────────────────────────────────

/// A signed community interpretation suitable for peer sync.
#[derive(Debug, Clone)]
pub struct CommunityEntry {
    /// Canonical key string, e.g. `"natal:jupiter_trine_venus"`.
    pub interp_key: String,
    pub body: String,
    pub author_pk: [u8; 32],
    /// ed25519 signature over `signing_payload(key, body)`.
    pub author_sig: [u8; 64],
}

/// A recently contributed community interpretation for the network activity feed.
#[derive(Debug, Clone)]
pub struct RecentInterp {
    /// Canonical key string, e.g. `"natal:jupiter_trine_venus"`.
    pub interp_key: String,
    pub body: String,
    pub received_at: u64,
}

// ── store ─────────────────────────────────────────────────────────────────────

/// Async, cloneable handle to the SQLite store.
///
/// Cheap to clone — the inner `SqlitePool` is reference-counted.
#[derive(Clone)]
pub struct ZodiaStore {
    pool: SqlitePool,
}

impl ZodiaStore {
    /// Open (or create) the SQLite database at `path`.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.init().await?;
        Ok(store)
    }

    /// In-memory database — useful for tests and first-run seeding checks.
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        // A single shared connection so all queries see the same in-memory DB.
        let opts = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.init().await?;
        Ok(store)
    }

    async fn init(&self) -> Result<(), StoreError> {
        for stmt in SCHEMA_STMTS {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        // Best-effort migrations for older DBs (column may already exist).
        let _ = sqlx::query("ALTER TABLE interpretations ADD COLUMN author_sig BLOB")
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("ALTER TABLE interpretations ADD COLUMN parent_log_id BLOB")
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("ALTER TABLE interpretations ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0")
            .execute(&self.pool)
            .await;
        // Phase F-collab veto rollback: rollback fields on interp_docs.
        for stmt in &[
            "ALTER TABLE interp_docs ADD COLUMN prior_snapshot BLOB",
            "ALTER TABLE interp_docs ADD COLUMN last_edit_op_id BLOB",
            "ALTER TABLE interp_docs ADD COLUMN last_edit_ts INTEGER",
            "ALTER TABLE interp_docs ADD COLUMN last_edit_author BLOB",
            "ALTER TABLE interp_docs ADD COLUMN last_edit_blocks BLOB",
        ] {
            let _ = sqlx::query(stmt).execute(&self.pool).await;
        }
        // AI-assisted-draft disclosure: ai_generated column on doc_block_authors.
        let _ = sqlx::query("ALTER TABLE doc_block_authors ADD COLUMN ai_generated INTEGER NOT NULL DEFAULT 0")
            .execute(&self.pool)
            .await;
        Ok(())
    }

    /// Borrow the underlying connection pool.  Exposed so callers in the same
    /// workspace can run small ad-hoc lookups (e.g. join the interpretations
    /// row keyed by `log_id`) without us shipping a dedicated method for
    /// every shape.  Kept terse — adding sql in the caller is the cost.
    pub fn pool_ref(&self) -> &SqlitePool {
        &self.pool
    }

    // ── signing payload ───────────────────────────────────────────────────────

    /// The bytes that must be ed25519-signed to produce a valid `author_sig`.
    ///
    /// Equal to `BLAKE3(key_sig_bytes || body_bytes)` — the same value used as
    /// `log_id`, so signing the log_id commits to both the key and the body.
    pub fn signing_payload(key: &InterpKey, body: &str) -> [u8; 32] {
        derive_log_id(&key.to_sig(), body)
    }

    // ── interpretations ───────────────────────────────────────────────────────

    /// Insert a baseline interpretation (no signature required).
    ///
    /// Duplicate log_ids are silently ignored (idempotent on re-seed).
    pub async fn insert_interpretation(
        &self,
        key: &InterpKey,
        body: &str,
        author_pk: Option<&[u8; 32]>,
        is_baseline: bool,
    ) -> Result<[u8; 32], StoreError> {
        let sig = key.to_sig();
        let log_id = derive_log_id(&sig, body);
        let now = unix_secs() as i64;
        let author_pk_vec = author_pk.map(|b| b.to_vec());
        sqlx::query(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, received_at, is_baseline)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(log_id.as_slice())
        .bind(&sig)
        .bind(kind_str(key))
        .bind(body)
        .bind(author_pk_vec)
        .bind(now)
        .bind(is_baseline as i32)
        .execute(&self.pool)
        .await?;
        Ok(log_id)
    }

    /// Insert a locally authored community interpretation with its ed25519 signature.
    ///
    /// The caller must sign `ZodiaStore::signing_payload(key, body)` with the
    /// author's identity key before calling this.
    pub async fn insert_signed(
        &self,
        key: &InterpKey,
        body: &str,
        author_pk: &[u8; 32],
        author_sig: &[u8; 64],
    ) -> Result<[u8; 32], StoreError> {
        let sig = key.to_sig();
        let log_id = derive_log_id(&sig, body);
        let now = unix_secs() as i64;
        sqlx::query(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, author_sig, received_at, is_baseline)
             VALUES (?, ?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(log_id.as_slice())
        .bind(&sig)
        .bind(kind_str(key))
        .bind(body)
        .bind(author_pk.as_slice())
        .bind(author_sig.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(log_id)
    }

    /// Persist an interpretation whose authentication is provided by an
    /// outer container (e.g. the p2panda operation header signature from a
    /// LogSync-replicated op) — no Zodia-level `author_sig` is required or
    /// stored.  The caller is responsible for asserting the row's
    /// authenticity before invoking this.
    ///
    /// Returns the derived `log_id`.  `INSERT OR IGNORE` semantics: duplicate
    /// log_ids are silently dropped.
    ///
    /// Rows inserted this way leave the `author_sig` column NULL, which
    /// means they don't participate in Tier-1 `community_for_keys` re-sharing
    /// (that path filters on `author_sig IS NOT NULL`).  Re-sharing happens
    /// via LogSync instead, which carries the original p2panda header.
    pub async fn insert_from_op(
        &self,
        interp_key: &str,
        body: &str,
        author_pk: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let log_id = derive_log_id(interp_key, body);
        let kind   = kind_from_key_str(interp_key);
        let now    = unix_secs() as i64;
        let result = sqlx::query(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, received_at, is_baseline)
             VALUES (?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(log_id.as_slice())
        .bind(interp_key)
        .bind(kind)
        .bind(body)
        .bind(author_pk.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Verify and insert a community interpretation received from a peer.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already present,
    /// `Err(StoreError::InvalidSignature)` if the signature does not verify.
    pub async fn insert_received(
        &self,
        interp_key: &str,
        body: &str,
        author_pk: &[u8; 32],
        author_sig: &[u8; 64],
    ) -> Result<bool, StoreError> {
        // Verify the ed25519 signature before writing anything.
        let payload = derive_log_id(interp_key, body);
        let vk = VerifyingKey::from_bytes(author_pk)
            .map_err(|_| StoreError::InvalidSignature)?;
        let sig = Signature::from_bytes(author_sig);
        vk.verify_strict(&payload, &sig)
            .map_err(|_| StoreError::InvalidSignature)?;

        let log_id = payload;
        let kind = kind_from_key_str(interp_key);
        let now = unix_secs() as i64;
        let result = sqlx::query(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, author_sig, received_at, is_baseline)
             VALUES (?, ?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(log_id.as_slice())
        .bind(interp_key)
        .bind(kind)
        .bind(body)
        .bind(author_pk.as_slice())
        .bind(author_sig.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Collect top non-baseline, signed community entries for the given canonical
    /// key strings.  Returns at most `limit` rows, sorted by affirmation count.
    ///
    /// Only entries with a valid `author_sig` are included — unsigned baseline
    /// entries and legacy unsigned community entries are excluded.
    pub async fn community_for_keys(
        &self,
        key_sigs: &[&str],
        limit: usize,
    ) -> Result<Vec<CommunityEntry>, StoreError> {
        if key_sigs.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = key_sigs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT interp_key, body, author_pk, author_sig
             FROM interpretations
             WHERE is_baseline = 0
               AND author_sig IS NOT NULL
               AND revoked = 0
               AND interp_key IN ({placeholders})
             ORDER BY (
                 SELECT COUNT(*) FROM affirmations
                 WHERE interp_log_id = interpretations.log_id
             ) DESC
             LIMIT ?"
        );

        let mut q = sqlx::query(&sql);
        for s in key_sigs {
            q = q.bind(*s);
        }
        q = q.bind(limit as i64);

        let rows = q.fetch_all(&self.pool).await?;
        let entries = rows
            .into_iter()
            .map(|row| {
                let pk_bytes: Vec<u8> = row.get(2);
                let sig_bytes: Vec<u8> = row.get(3);
                let mut author_pk = [0u8; 32];
                let mut author_sig = [0u8; 64];
                author_pk.copy_from_slice(&pk_bytes[..32.min(pk_bytes.len())]);
                author_sig.copy_from_slice(&sig_bytes[..64.min(sig_bytes.len())]);
                CommunityEntry {
                    interp_key: row.get(0),
                    body: row.get(1),
                    author_pk,
                    author_sig,
                }
            })
            .collect();
        Ok(entries)
    }

    /// The single best interpretation for a key — community-contributed first,
    /// then sorted by affirmation count, with baseline as fallback.
    pub async fn top_body(&self, key: &InterpKey) -> Result<Option<String>, StoreError> {
        let sig = key.to_sig();
        let row = sqlx::query(
            "SELECT body FROM interpretations
             WHERE interp_key = ? AND revoked = 0
             ORDER BY is_baseline ASC,
                      (SELECT COUNT(*) FROM affirmations
                       WHERE interp_log_id = interpretations.log_id) DESC
             LIMIT 1",
        )
        .bind(&sig)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<String, _>(0)))
    }

    /// All interpretations for a key, best-first.
    pub async fn all_for_key(&self, key: &InterpKey) -> Result<Vec<InterpRow>, StoreError> {
        let sig = key.to_sig();
        let rows = sqlx::query(
            "SELECT log_id, body, author_pk, received_at, is_baseline,
                    (SELECT COUNT(*) FROM affirmations WHERE interp_log_id = i.log_id) AS aff_count
             FROM interpretations i
             WHERE interp_key = ? AND revoked = 0
             ORDER BY is_baseline ASC, aff_count DESC",
        )
        .bind(&sig)
        .fetch_all(&self.pool)
        .await?;

        let out = rows
            .into_iter()
            .map(|row| {
                let log_id_bytes: Vec<u8> = row.get(0);
                let mut log_id = [0u8; 32];
                log_id.copy_from_slice(&log_id_bytes[..32.min(log_id_bytes.len())]);
                let author_bytes: Option<Vec<u8>> = row.get(2);
                let author_pk = author_bytes.and_then(|b| {
                    if b.len() == 32 {
                        let mut a = [0u8; 32];
                        a.copy_from_slice(&b);
                        Some(a)
                    } else {
                        None
                    }
                });
                InterpRow {
                    log_id,
                    body: row.get(1),
                    author_pk,
                    received_at: row.get::<i64, _>(3) as u64,
                    is_baseline: row.get::<i32, _>(4) != 0,
                    affirmation_count: row.get::<i64, _>(5) as u64,
                }
            })
            .collect();
        Ok(out)
    }

    /// Most recently received/authored community interpretations, newest first.
    pub async fn recent_community_interps(
        &self,
        limit: usize,
    ) -> Result<Vec<RecentInterp>, StoreError> {
        let rows = sqlx::query(
            "SELECT interp_key, body, received_at FROM interpretations
             WHERE is_baseline = 0 AND author_sig IS NOT NULL
             ORDER BY received_at DESC
             LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| RecentInterp {
                interp_key: row.get(0),
                body: row.get(1),
                received_at: row.get::<i64, _>(2) as u64,
            })
            .collect())
    }

    /// Number of non-baseline interpretations in the store.
    pub async fn community_count(&self) -> Result<u64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) FROM interpretations WHERE is_baseline = 0")
            .fetch_one(&self.pool)
            .await?;
        let n: i64 = row.get(0);
        Ok(n as u64)
    }

    // ── affirmations ──────────────────────────────────────────────────────────

    /// Record an affirmation.  Returns `Ok(true)` if newly inserted, `Ok(false)`
    /// if this author had already affirmed this interpretation.
    pub async fn affirm(
        &self,
        interp_log_id: &[u8; 32],
        author_pk: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(interp_log_id);
        hasher.update(author_pk);
        let log_id: [u8; 32] = *hasher.finalize().as_bytes();
        let now = unix_secs() as i64;
        let result = sqlx::query(
            "INSERT OR IGNORE INTO affirmations (log_id, interp_log_id, author_pk, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(log_id.as_slice())
        .bind(interp_log_id.as_slice())
        .bind(author_pk.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Affirmation count for one interpretation.
    pub async fn affirmation_count(&self, log_id: &[u8; 32]) -> Result<u64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) FROM affirmations WHERE interp_log_id = ?")
            .bind(log_id.as_slice())
            .fetch_one(&self.pool)
            .await?;
        let n: i64 = row.get(0);
        Ok(n as u64)
    }

    // ── messages ──────────────────────────────────────────────────────────────

    /// Persist a single chat message.
    pub async fn insert_message(
        &self,
        peer_id: &[u8; 32],
        from_us: bool,
        body: &str,
    ) -> Result<(), StoreError> {
        let ts = unix_ms() as i64;
        sqlx::query("INSERT INTO messages (peer_id, from_us, body, ts) VALUES (?, ?, ?, ?)")
            .bind(peer_id.as_slice())
            .bind(from_us as i32)
            .bind(body)
            .bind(ts)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load all messages for a peer, oldest-first.
    pub async fn messages_for_peer(
        &self,
        peer_id: &[u8; 32],
    ) -> Result<Vec<(bool, String)>, StoreError> {
        let rows = sqlx::query(
            "SELECT from_us, body FROM messages WHERE peer_id = ? ORDER BY ts ASC, id ASC",
        )
        .bind(peer_id.as_slice())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let from_us: i32 = row.get(0);
                let body: String = row.get(1);
                (from_us != 0, body)
            })
            .collect())
    }

    // ── peer display names ────────────────────────────────────────────────────

    /// Upsert `name` for `peer_pk`, but only if `updated_at` is newer than
    /// whatever's already on file — last-writer-wins, guards against an
    /// out-of-order redelivery clobbering a more recent name with a stale
    /// one. Returns whether the row was actually written.
    pub async fn set_peer_display_name_if_newer(
        &self,
        peer_pk:    &[u8; 32],
        name:       &str,
        updated_at: u64,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "INSERT INTO peer_display_names (peer_pk, name, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(peer_pk) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at
             WHERE excluded.updated_at > peer_display_names.updated_at",
        )
        .bind(peer_pk.as_slice())
        .bind(name)
        .bind(updated_at as i64)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The display name a peer has broadcast for themself, if any.
    pub async fn peer_display_name(&self, peer_pk: &[u8; 32]) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT name FROM peer_display_names WHERE peer_pk = ?")
            .bind(peer_pk.as_slice())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Every known peer display name, keyed by pubkey — for bulk-loading
    /// into an in-memory lookup on startup rather than querying per-row.
    pub async fn all_peer_display_names(&self) -> Result<Vec<([u8; 32], String)>, StoreError> {
        let rows = sqlx::query("SELECT peer_pk, name FROM peer_display_names")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let pk: Vec<u8> = row.get(0);
                let name: String = row.get(1);
                <[u8; 32]>::try_from(pk.as_slice()).ok().map(|pk| (pk, name))
            })
            .collect())
    }

    // ── last seen ──────────────────────────────────────────────────────────────

    /// Record that `peer_pk`'s direct channel just closed, at `seen_at`.
    /// Upsert — a later close always overwrites an earlier one.
    pub async fn record_last_seen(&self, peer_pk: &[u8; 32], seen_at: u64) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO peer_last_seen (peer_pk, seen_at) VALUES (?, ?)
             ON CONFLICT(peer_pk) DO UPDATE SET seen_at = excluded.seen_at",
        )
        .bind(peer_pk.as_slice())
        .bind(seen_at as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every known "last seen" timestamp, keyed by pubkey — for bulk-loading
    /// into an in-memory lookup on startup, same reasoning as
    /// `all_peer_display_names`.
    pub async fn all_last_seen(&self) -> Result<Vec<([u8; 32], u64)>, StoreError> {
        let rows = sqlx::query("SELECT peer_pk, seen_at FROM peer_last_seen")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let pk: Vec<u8> = row.get(0);
                let seen_at: i64 = row.get(1);
                <[u8; 32]>::try_from(pk.as_slice()).ok().map(|pk| (pk, seen_at as u64))
            })
            .collect())
    }

    // ── muted peers ────────────────────────────────────────────────────────────

    /// Mute `peer_pk`'s social activity in the live feed. Idempotent.
    pub async fn mute_peer(&self, peer_pk: &[u8; 32]) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO muted_peers (peer_pk, muted_at) VALUES (?, ?)
             ON CONFLICT(peer_pk) DO NOTHING",
        )
        .bind(peer_pk.as_slice())
        .bind(unix_secs() as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unmute `peer_pk`. Idempotent — a no-op if they weren't muted.
    pub async fn unmute_peer(&self, peer_pk: &[u8; 32]) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM muted_peers WHERE peer_pk = ?")
            .bind(peer_pk.as_slice())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every currently-muted peer, for bulk-loading into an in-memory
    /// lookup on startup — same reasoning as `all_peer_display_names`.
    pub async fn muted_peers(&self) -> Result<Vec<[u8; 32]>, StoreError> {
        let rows = sqlx::query("SELECT peer_pk FROM muted_peers")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let pk: Vec<u8> = row.get(0);
                <[u8; 32]>::try_from(pk.as_slice()).ok()
            })
            .collect())
    }

    // ── responses (causal threads) ────────────────────────────────────────────

    /// Persist a response that hangs off `parent_log_id`.  Uses the same
    /// content-hash log_id derivation as a plain authored interpretation, so
    /// affirmations on responses Just Work via the existing affirmations table.
    ///
    /// `parent_log_id` is stored verbatim; orphan responses (parent not yet
    /// known locally) are still persisted so the join-on-display picks them up
    /// when the parent eventually arrives via sync.
    ///
    /// Returns the response's own `log_id`.
    pub async fn insert_response_from_op(
        &self,
        parent_log_id: &[u8; 32],
        body:          &str,
        author_pk:     &[u8; 32],
    ) -> Result<bool, StoreError> {
        // The response shares the parent's `interp_key` for ranking/lookups —
        // it's part of the same conversation about that key.  We look that up
        // from the parent row; if the parent is unknown, we still insert with
        // `interp_key` = "" and `interp_kind` = "response_orphan" so the row
        // is materialised and can be reconciled later.
        let parent_key: Option<(String, String)> = sqlx::query_as(
            "SELECT interp_key, interp_kind FROM interpretations WHERE log_id = ?",
        )
        .bind(parent_log_id.as_slice())
        .fetch_optional(&self.pool)
        .await?;

        let (interp_key, kind) = match parent_key {
            Some((k, _)) => (k.clone(), "response".to_string()),
            None         => (String::new(), "response_orphan".to_string()),
        };

        let log_id = derive_log_id(&interp_key, body);
        let now    = unix_secs() as i64;
        let result = sqlx::query(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, parent_log_id, received_at, is_baseline)
             VALUES (?, ?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(log_id.as_slice())
        .bind(&interp_key)
        .bind(&kind)
        .bind(body)
        .bind(author_pk.as_slice())
        .bind(parent_log_id.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// All responses authored under `parent_log_id`, oldest-first.
    pub async fn responses_for(
        &self,
        parent_log_id: &[u8; 32],
    ) -> Result<Vec<InterpRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT log_id, body, author_pk, received_at, is_baseline,
                    (SELECT COUNT(*) FROM affirmations WHERE interp_log_id = i.log_id) AS aff_count
             FROM interpretations i
             WHERE parent_log_id = ?
             ORDER BY received_at ASC",
        )
        .bind(parent_log_id.as_slice())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_interp_row).collect())
    }

    // ── feed item synthesis (Phase E) ─────────────────────────────────────────

    /// Synthesize recent feed items from existing op tables.  Returns events
    /// (authored interps, affirmations, responses) tagged with the local
    /// identity `me` so the caller can compute `targets_me` against it.
    ///
    /// Newest-first, capped at `limit` rows.  Used when Sky / per-aspect pages
    /// first paint, before live `StateEvent`s start flowing.
    pub async fn recent_feed_rows(
        &self,
        me: &[u8; 32],
        limit: usize,
    ) -> Result<Vec<FeedRow>, StoreError> {
        // UNION ALL of three event flavours.  Each row carries its event_id
        // (= log_id), kind discriminator, key/body, author, optional parent
        // (for responses) / target log_id (for affirms), and timestamp.
        let lim = limit as i64;
        let rows = sqlx::query(
            "SELECT * FROM (
                SELECT log_id AS event_id, 'authored' AS kind,
                       interp_key, body, author_pk,
                       NULL AS parent_log_id, NULL AS target_log_id,
                       received_at AS ts
                  FROM interpretations
                 WHERE is_baseline = 0 AND parent_log_id IS NULL AND revoked = 0
                 ORDER BY received_at DESC LIMIT ?1
              ) UNION ALL SELECT * FROM (
                SELECT log_id AS event_id, 'response' AS kind,
                       interp_key, body, author_pk,
                       parent_log_id, NULL AS target_log_id,
                       received_at AS ts
                  FROM interpretations
                 WHERE is_baseline = 0 AND parent_log_id IS NOT NULL AND revoked = 0
                 ORDER BY received_at DESC LIMIT ?1
              ) UNION ALL SELECT * FROM (
                SELECT a.log_id AS event_id, 'affirm' AS kind,
                       i.interp_key AS interp_key, i.body AS body,
                       a.author_pk, NULL AS parent_log_id,
                       a.interp_log_id AS target_log_id,
                       a.created_at AS ts
                  FROM affirmations a
                  JOIN interpretations i ON i.log_id = a.interp_log_id
                 WHERE i.revoked = 0
                 ORDER BY a.created_at DESC LIMIT ?1
              )
              ORDER BY ts DESC LIMIT ?1",
        )
        .bind(lim)
        .fetch_all(&self.pool)
        .await?;

        let me_bytes = me.as_slice();
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let event_id_bytes: Vec<u8> = row.get(0);
                if event_id_bytes.len() != 32 { return None; }
                let mut event_id = [0u8; 32];
                event_id.copy_from_slice(&event_id_bytes);
                let kind: String = row.get(1);
                let interp_key: String = row.get(2);
                let body: String = row.get(3);
                let author_bytes: Option<Vec<u8>> = row.get(4);
                let author_pk = author_bytes.and_then(|b| {
                    if b.len() == 32 { let mut a = [0u8; 32]; a.copy_from_slice(&b); Some(a) }
                    else { None }
                });
                let parent_bytes: Option<Vec<u8>> = row.get(5);
                let parent_log_id = parent_bytes.and_then(|b| {
                    if b.len() == 32 { let mut a = [0u8; 32]; a.copy_from_slice(&b); Some(a) }
                    else { None }
                });
                let target_bytes: Option<Vec<u8>> = row.get(6);
                let target_log_id = target_bytes.and_then(|b| {
                    if b.len() == 32 { let mut a = [0u8; 32]; a.copy_from_slice(&b); Some(a) }
                    else { None }
                });
                let ts: i64 = row.get(7);
                Some(FeedRow {
                    event_id,
                    kind: match kind.as_str() {
                        "authored" => FeedRowKind::Authored,
                        "response" => FeedRowKind::Response,
                        "affirm"   => FeedRowKind::Affirm,
                        _          => return None,
                    },
                    interp_key,
                    body,
                    author_pk,
                    parent_log_id,
                    target_log_id,
                    ts: ts as u64,
                    author_is_me: author_pk.as_ref().map(|a| a.as_slice() == me_bytes).unwrap_or(false),
                })
            })
            .collect())
    }

    // ── feed read-state (Phase E) ─────────────────────────────────────────────

    /// Mark a single feed event as read by id.  Idempotent — repeated calls
    /// update `read_at` to the latest timestamp.
    pub async fn mark_event_read(&self, event_id: &[u8; 32]) -> Result<(), StoreError> {
        let now = unix_secs() as i64;
        sqlx::query(
            "INSERT INTO feed_read (event_id, read_at) VALUES (?, ?)
             ON CONFLICT(event_id) DO UPDATE SET read_at = excluded.read_at",
        )
        .bind(event_id.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a read-state row, returning the event to unread state.
    pub async fn mark_event_unread(&self, event_id: &[u8; 32]) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM feed_read WHERE event_id = ?")
            .bind(event_id.as_slice())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// True iff `event_id` is currently marked read.
    pub async fn is_event_read(&self, event_id: &[u8; 32]) -> Result<bool, StoreError> {
        let row = sqlx::query("SELECT 1 FROM feed_read WHERE event_id = ?")
            .bind(event_id.as_slice())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// Bulk-mark a set of event ids as read.  Used when the user clicks the
    /// notification bell to acknowledge all pending targeting events at once.
    /// Returns how many rows were newly inserted.
    pub async fn bulk_mark_read(
        &self,
        event_ids: &[[u8; 32]],
    ) -> Result<u64, StoreError> {
        if event_ids.is_empty() {
            return Ok(0);
        }
        let now = unix_secs() as i64;
        let mut tx = self.pool.begin().await?;
        let mut inserted = 0u64;
        for id in event_ids {
            let result = sqlx::query(
                "INSERT OR IGNORE INTO feed_read (event_id, read_at) VALUES (?, ?)",
            )
            .bind(id.as_slice())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            inserted += result.rows_affected();
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// Persist a small key/value pair in `feed_meta`.  Used by the
    /// `TransitTicker` to remember the previous tick's in-orb set across
    /// restarts.
    pub async fn set_feed_meta(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO feed_meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load a previously-stored `feed_meta` value, or `None` if unset.
    pub async fn get_feed_meta(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let row = sqlx::query("SELECT value FROM feed_meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>(0)))
    }

    /// Count of "events targeting me" that are still unread.  An event targets
    /// the local identity iff it is an affirmation or a response on an interp
    /// the local identity authored.  Used to badge the notification bell.
    ///
    /// `me` is the local p2panda verifying-key bytes (the `author_pk` columns
    /// hold these).  Note: rows authored locally are excluded — your own
    /// affirmations/responses on your own work don't badge.
    pub async fn feed_targeting_unread_count(&self, me: &[u8; 32]) -> Result<u64, StoreError> {
        // Affirmations: an affirmation targets me iff its interp_log_id points
        // to an interpretation whose author_pk == me, and the voter is not me.
        // Each affirmation row's event_id (for read-state purposes) is its
        // BLAKE3-derived primary key (`log_id`), the same value the affirm
        // p2panda op hashes to downstream.
        //
        // Responses: a response targets me iff its parent_log_id points to an
        // interpretation whose author_pk == me, and the response author isn't
        // me.  The response's row log_id is its event id.
        let row = sqlx::query(
            "SELECT
                (SELECT COUNT(*) FROM affirmations a
                  JOIN interpretations i ON i.log_id = a.interp_log_id
                 WHERE i.author_pk = ?1
                   AND a.author_pk != ?1
                   AND a.log_id NOT IN (SELECT event_id FROM feed_read))
              +
                (SELECT COUNT(*) FROM interpretations r
                  JOIN interpretations p ON p.log_id = r.parent_log_id
                 WHERE r.parent_log_id IS NOT NULL
                   AND p.author_pk = ?1
                   AND r.author_pk != ?1
                   AND r.log_id NOT IN (SELECT event_id FROM feed_read))
             AS total",
        )
        .bind(me.as_slice())
        .fetch_one(&self.pool)
        .await?;
        let n: i64 = row.get(0);
        Ok(n as u64)
    }

    /// Event ids of all targeting events that are currently unread.  Used by
    /// the bell click-handler to bulk-mark them as read.
    pub async fn feed_targeting_unread_ids(&self, me: &[u8; 32]) -> Result<Vec<[u8; 32]>, StoreError> {
        let rows = sqlx::query(
            "SELECT a.log_id FROM affirmations a
              JOIN interpretations i ON i.log_id = a.interp_log_id
             WHERE i.author_pk = ?1
               AND a.author_pk != ?1
               AND a.log_id NOT IN (SELECT event_id FROM feed_read)
             UNION ALL
             SELECT r.log_id FROM interpretations r
              JOIN interpretations p ON p.log_id = r.parent_log_id
             WHERE r.parent_log_id IS NOT NULL
               AND p.author_pk = ?1
               AND r.author_pk != ?1
               AND r.log_id NOT IN (SELECT event_id FROM feed_read)",
        )
        .bind(me.as_slice())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let bytes: Vec<u8> = row.get(0);
                if bytes.len() == 32 {
                    let mut id = [0u8; 32];
                    id.copy_from_slice(&bytes);
                    Some(id)
                } else {
                    None
                }
            })
            .collect())
    }

    /// Mark an interpretation as revoked.  Authorization is the caller's
    /// responsibility — pass `expected_author` so the row is only tombstoned
    /// when the requesting actor matches the original author.  Returns true
    /// if a row was newly revoked, false if no match (wrong author or
    /// unknown log_id) or already revoked.
    pub async fn revoke_interp(
        &self,
        log_id:          &[u8; 32],
        expected_author: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE interpretations
                SET revoked = 1
              WHERE log_id = ?1
                AND author_pk = ?2
                AND revoked = 0",
        )
        .bind(log_id.as_slice())
        .bind(expected_author.as_slice())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Look up the `(interp_key, author_pk)` of one interpretation by its log_id.
    /// Used by feed-routing code to derive `targets_me` and `interp_key` for
    /// affirmation / response events.
    pub async fn interp_key_and_author(
        &self,
        log_id: &[u8; 32],
    ) -> Result<Option<(String, Option<[u8; 32]>)>, StoreError> {
        let row = sqlx::query(
            "SELECT interp_key, author_pk FROM interpretations WHERE log_id = ?",
        )
        .bind(log_id.as_slice())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let key: String = r.get(0);
            let author_bytes: Option<Vec<u8>> = r.get(1);
            let author = author_bytes.and_then(|b| {
                if b.len() == 32 { let mut a = [0u8; 32]; a.copy_from_slice(&b); Some(a) }
                else { None }
            });
            (key, author)
        }))
    }

    // ── collaborative docs (Phase F-collab) ───────────────────────────────────

    /// Load the persisted Loro snapshot for `interp_key`, if one exists.
    /// Returned bytes feed `zodia_doc::InterpDoc::from_snapshot`.
    pub async fn doc_load(&self, interp_key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let row = sqlx::query("SELECT loro_snapshot FROM interp_docs WHERE interp_key = ?")
            .bind(interp_key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>(0)))
    }

    /// Replace (or insert) the doc snapshot for `interp_key`.
    pub async fn doc_save(
        &self,
        interp_key:   &str,
        snapshot:     &[u8],
        snapshot_rev: &[u8; 32],
    ) -> Result<(), StoreError> {
        let now = unix_secs() as i64;
        sqlx::query(
            "INSERT INTO interp_docs (interp_key, loro_snapshot, snapshot_rev, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(interp_key) DO UPDATE SET
                 loro_snapshot = excluded.loro_snapshot,
                 snapshot_rev  = excluded.snapshot_rev,
                 updated_at    = excluded.updated_at",
        )
        .bind(interp_key)
        .bind(snapshot)
        .bind(snapshot_rev.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Save snapshot AND record rollback metadata in one row update.  `prior`
    /// is the snapshot before this edit was applied (used by veto rollback);
    /// `last_edit_*` describe the edit just applied so a later `DocOp::Veto`
    /// can be authority-checked.  `blocks` is CBOR-encoded `Vec<[u8; 16]>`.
    pub async fn doc_save_with_history(
        &self,
        interp_key:        &str,
        snapshot:          &[u8],
        snapshot_rev:      &[u8; 32],
        prior:             Option<&[u8]>,
        last_edit_op_id:   &[u8; 32],
        last_edit_ts:      u64,
        last_edit_author:  &[u8; 32],
        last_edit_blocks:  &[u8],
    ) -> Result<(), StoreError> {
        let now = unix_secs() as i64;
        sqlx::query(
            "INSERT INTO interp_docs (
                interp_key, loro_snapshot, snapshot_rev, updated_at,
                prior_snapshot, last_edit_op_id, last_edit_ts,
                last_edit_author, last_edit_blocks
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(interp_key) DO UPDATE SET
                loro_snapshot    = excluded.loro_snapshot,
                snapshot_rev     = excluded.snapshot_rev,
                updated_at       = excluded.updated_at,
                prior_snapshot   = excluded.prior_snapshot,
                last_edit_op_id  = excluded.last_edit_op_id,
                last_edit_ts     = excluded.last_edit_ts,
                last_edit_author = excluded.last_edit_author,
                last_edit_blocks = excluded.last_edit_blocks",
        )
        .bind(interp_key)
        .bind(snapshot)
        .bind(snapshot_rev.as_slice())
        .bind(now)
        .bind(prior)
        .bind(last_edit_op_id.as_slice())
        .bind(last_edit_ts as i64)
        .bind(last_edit_author.as_slice())
        .bind(last_edit_blocks)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read the rollback metadata for `interp_key` (newest edit + prior
    /// snapshot).  `None` if no doc row, `Some(meta)` with optional fields
    /// otherwise.  `last_edit_*` may all be `None` if the doc exists but no
    /// vetoable edit landed yet (e.g. migration-seeded only).
    pub async fn doc_load_meta(
        &self,
        interp_key: &str,
    ) -> Result<Option<DocMeta>, StoreError> {
        let row = sqlx::query(
            "SELECT prior_snapshot, last_edit_op_id, last_edit_ts,
                    last_edit_author, last_edit_blocks
               FROM interp_docs WHERE interp_key = ?",
        )
        .bind(interp_key)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None); };
        let prior:  Option<Vec<u8>> = r.get(0);
        let op_id:  Option<Vec<u8>> = r.get(1);
        let ts:     Option<i64>     = r.get(2);
        let author: Option<Vec<u8>> = r.get(3);
        let blocks: Option<Vec<u8>> = r.get(4);
        let op_id_arr = op_id.and_then(|b|
            if b.len() == 32 { let mut a = [0u8; 32]; a.copy_from_slice(&b); Some(a) }
            else { None });
        let author_arr = author.and_then(|b|
            if b.len() == 32 { let mut a = [0u8; 32]; a.copy_from_slice(&b); Some(a) }
            else { None });
        Ok(Some(DocMeta {
            prior_snapshot:   prior,
            last_edit_op_id:  op_id_arr,
            last_edit_ts:     ts.map(|v| v as u64),
            last_edit_author: author_arr,
            last_edit_blocks: blocks,
        }))
    }

    /// Apply rollback: restore `prior_snapshot` as current snapshot, clear
    /// last_edit_* metadata, and pop the newest entry from each provided
    /// block ring.  Returns `true` if rollback ran, `false` if doc has no
    /// recorded prior snapshot.
    pub async fn doc_rollback(
        &self,
        interp_key:       &str,
        snapshot_rev:     &[u8; 32],
        affected_blocks:  &[[u8; 16]],
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT prior_snapshot FROM interp_docs WHERE interp_key = ?",
        )
        .bind(interp_key)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(r) = row else { return Ok(false); };
        let prior: Option<Vec<u8>> = r.get(0);
        let Some(prior_bytes) = prior else { return Ok(false); };
        let now = unix_secs() as i64;
        sqlx::query(
            "UPDATE interp_docs SET
                loro_snapshot    = ?,
                snapshot_rev     = ?,
                updated_at       = ?,
                prior_snapshot   = NULL,
                last_edit_op_id  = NULL,
                last_edit_ts     = NULL,
                last_edit_author = NULL,
                last_edit_blocks = NULL
             WHERE interp_key = ?",
        )
        .bind(prior_bytes.as_slice())
        .bind(snapshot_rev.as_slice())
        .bind(now)
        .bind(interp_key)
        .execute(&mut *tx)
        .await?;
        // Pop newest ring entry per affected block.
        for block_id in affected_blocks {
            let pos: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(position) FROM doc_block_authors
                  WHERE interp_key = ? AND block_id = ?",
            )
            .bind(interp_key)
            .bind(block_id.as_slice())
            .fetch_one(&mut *tx)
            .await?;
            if let Some(p) = pos {
                sqlx::query(
                    "DELETE FROM doc_block_authors
                      WHERE interp_key = ? AND block_id = ? AND position = ?",
                )
                .bind(interp_key)
                .bind(block_id.as_slice())
                .bind(p)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Read the author ring for one (interp_key, block_id).  Returns the
    /// entries in FIFO order (oldest first); the caller's `Ring` builder
    /// re-sorts as needed.
    /// Whether `author_pk` has any block currently in `interp_key`'s doc
    /// author-veto ring — i.e. some text they wrote is still present in
    /// the doc's converged body. Used to decide whether an affirmation on
    /// a doc should count as "targets me" for feed/bell purposes: the
    /// collaborative doc model has no single "author" the way the legacy
    /// `InterpOp::Author` model did, so "did I contribute anything still
    /// visible in this doc" is the closest equivalent notion of ownership.
    pub async fn doc_has_contributor(
        &self,
        interp_key: &str,
        author_pk:  &[u8; 32],
    ) -> Result<bool, StoreError> {
        let row = sqlx::query(
            "SELECT 1 FROM doc_block_authors WHERE interp_key = ? AND author_pk = ? LIMIT 1",
        )
        .bind(interp_key)
        .bind(author_pk.as_slice())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn block_ring_get(
        &self,
        interp_key: &str,
        block_id:   &[u8; 16],
    ) -> Result<Vec<(/*author*/[u8; 32], /*edit_op_id*/[u8; 32], /*edited_at*/u64, /*ai_generated*/bool)>, StoreError> {
        let rows = sqlx::query(
            "SELECT author_pk, edit_op_id, edited_at, ai_generated
               FROM doc_block_authors
              WHERE interp_key = ? AND block_id = ?
              ORDER BY position ASC",
        )
        .bind(interp_key)
        .bind(block_id.as_slice())
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let a: Vec<u8> = row.get(0);
            let e: Vec<u8> = row.get(1);
            if a.len() != 32 || e.len() != 32 { continue; }
            let mut author = [0u8; 32]; author.copy_from_slice(&a);
            let mut op_id  = [0u8; 32]; op_id .copy_from_slice(&e);
            let ai_generated: i64 = row.get(3);
            out.push((author, op_id, row.get::<i64, _>(2) as u64, ai_generated != 0));
        }
        Ok(out)
    }

    /// Append a new author to the ring for one (interp_key, block_id),
    /// evicting the oldest entry if capacity (5) is reached.  Reads,
    /// shifts in memory, writes back in a transaction.  Caller-provided
    /// `now_unix` keeps the call deterministic in tests.
    pub async fn block_ring_push(
        &self,
        interp_key:   &str,
        block_id:     &[u8; 16],
        author:       &[u8; 32],
        edit_op_id:   &[u8; 32],
        now_unix:     u64,
        ai_generated: bool,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        // Load current entries.
        let rows = sqlx::query(
            "SELECT author_pk, edit_op_id, edited_at, ai_generated
               FROM doc_block_authors
              WHERE interp_key = ? AND block_id = ?
              ORDER BY position ASC",
        )
        .bind(interp_key)
        .bind(block_id.as_slice())
        .fetch_all(&mut *tx)
        .await?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>, i64, i64)> = rows
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get::<i64, _>(2), r.get::<i64, _>(3)))
            .collect();
        entries.push((author.to_vec(), edit_op_id.to_vec(), now_unix as i64, ai_generated as i64));
        while entries.len() > zodia_doc::RING_SIZE {
            entries.remove(0);
        }
        // Delete + reinsert atomically.
        sqlx::query("DELETE FROM doc_block_authors WHERE interp_key = ? AND block_id = ?")
            .bind(interp_key)
            .bind(block_id.as_slice())
            .execute(&mut *tx)
            .await?;
        for (pos, (a, e, t, ai)) in entries.iter().enumerate() {
            sqlx::query(
                "INSERT INTO doc_block_authors
                    (interp_key, block_id, position, author_pk, edit_op_id, edited_at, ai_generated)
                  VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(interp_key)
            .bind(block_id.as_slice())
            .bind(pos as i64)
            .bind(a)
            .bind(e)
            .bind(*t)
            .bind(*ai)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Whether `interp_key`'s doc currently has any block whose ring
    /// includes an AI-drafted edit — used to show a disclosure caption on
    /// the reading. Deliberately "any block, any ring position" rather
    /// than just the newest: even an older-but-still-present block's text
    /// was AI-drafted and may still be part of the converged body.
    pub async fn doc_has_ai_generated_content(&self, interp_key: &str) -> Result<bool, StoreError> {
        let row = sqlx::query(
            "SELECT 1 FROM doc_block_authors WHERE interp_key = ? AND ai_generated = 1 LIMIT 1",
        )
        .bind(interp_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Record one affirmation against a (interp_key, revision) pair.
    /// Idempotent on duplicate (voter, rev).
    pub async fn doc_affirm_rev(
        &self,
        interp_key: &str,
        target_rev: &[u8; 32],
        voter_pk:   &[u8; 32],
    ) -> Result<bool, StoreError> {
        let now = unix_secs() as i64;
        let r = sqlx::query(
            "INSERT OR IGNORE INTO doc_affirms (interp_key, target_rev, voter_pk, affirmed_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(interp_key)
        .bind(target_rev.as_slice())
        .bind(voter_pk.as_slice())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Count of affirmations targeting one (interp_key, revision).
    pub async fn doc_affirm_count(
        &self,
        interp_key: &str,
        target_rev: &[u8; 32],
    ) -> Result<u64, StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*) FROM doc_affirms WHERE interp_key = ? AND target_rev = ?",
        )
        .bind(interp_key)
        .bind(target_rev.as_slice())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>(0) as u64)
    }

    /// Has the collab-doc migration already run?  Idempotent guard for the
    /// one-time `migrate_interpretations_to_docs` pass.  Implemented as a
    /// `feed_meta` flag so we don't need yet another tiny table.
    pub async fn collab_doc_migration_done(&self) -> Result<bool, StoreError> {
        Ok(self.get_feed_meta("collab_doc_migration_v1").await?.is_some())
    }

    pub async fn mark_collab_doc_migration_done(&self) -> Result<(), StoreError> {
        self.set_feed_meta("collab_doc_migration_v1", b"1").await
    }

    /// Distinct `interp_key` values present in the legacy `interpretations`
    /// table.  Used by migration to fold each key's competing rows into one
    /// collab doc.
    pub async fn distinct_interp_keys(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT interp_key FROM interpretations
              WHERE is_baseline = 0 AND revoked = 0",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
    }

    /// All (body, author_pk, received_at) rows for one key, oldest first.
    /// Used by migration to seed the per-key collab doc with each authored
    /// row attributed to its original author.
    pub async fn authored_rows_for_key(
        &self,
        interp_key: &str,
    ) -> Result<Vec<(String, [u8; 32], u64)>, StoreError> {
        let rows = sqlx::query(
            "SELECT body, author_pk, received_at
               FROM interpretations
              WHERE interp_key = ? AND is_baseline = 0
                AND revoked = 0 AND parent_log_id IS NULL
              ORDER BY received_at ASC",
        )
        .bind(interp_key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(|r| {
            let body: String = r.get(0);
            let pk: Vec<u8>  = r.get(1);
            if pk.len() != 32 { return None; }
            let mut author = [0u8; 32]; author.copy_from_slice(&pk);
            Some((body, author, r.get::<i64, _>(2) as u64))
        }).collect())
    }

    // ── migration ─────────────────────────────────────────────────────────────

    /// Delete all legacy seeded baseline rows. Idempotent — safe every startup.
    pub async fn scrub_baseline(&self) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM interpretations WHERE is_baseline = 1")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

}

// ── row decoder ───────────────────────────────────────────────────────────────

fn row_to_interp_row(row: sqlx::sqlite::SqliteRow) -> InterpRow {
    use sqlx::Row as _;
    let log_id_bytes: Vec<u8> = row.get(0);
    let mut log_id = [0u8; 32];
    log_id.copy_from_slice(&log_id_bytes[..32.min(log_id_bytes.len())]);
    let author_bytes: Option<Vec<u8>> = row.get(2);
    let author_pk = author_bytes.and_then(|b| {
        if b.len() == 32 {
            let mut a = [0u8; 32];
            a.copy_from_slice(&b);
            Some(a)
        } else {
            None
        }
    });
    InterpRow {
        log_id,
        body: row.get(1),
        author_pk,
        received_at: row.get::<i64, _>(3) as u64,
        is_baseline: row.get::<i32, _>(4) != 0,
        affirmation_count: row.get::<i64, _>(5) as u64,
    }
}

// ── BaselineStore: row synthesis ──────────────────────────────────────────────

impl seed::BaselineStore {
    /// Synthesise an `InterpRow` for the baseline entry for `key`, if present.
    ///
    /// The `log_id` uses the same BLAKE3 derivation as the old seeder, so any
    /// affirmations already recorded in legacy databases remain consistent.
    pub fn row_for_key(&self, key: &InterpKey) -> Option<InterpRow> {
        let body = self.lookup(key)?;
        let sig = key.to_sig();
        Some(InterpRow {
            log_id: derive_log_id(&sig, body),
            body: body.to_owned(),
            author_pk: None,
            received_at: 0,
            is_baseline: true,
            affirmation_count: 0,
        })
    }
}

// ── row type ──────────────────────────────────────────────────────────────────

/// A single row from the interpretations table, including affirmation count.
#[derive(Debug, Clone)]
pub struct InterpRow {
    pub log_id: [u8; 32],
    pub body: String,
    pub author_pk: Option<[u8; 32]>,
    pub received_at: u64,
    pub is_baseline: bool,
    pub affirmation_count: u64,
}

// ── feed row (Phase E) ────────────────────────────────────────────────────────

/// Rollback metadata read back from `interp_docs`.  All fields can be `None`
/// when no edit has landed since the doc was created (e.g. migration-seeded).
#[derive(Debug, Clone, Default)]
pub struct DocMeta {
    pub prior_snapshot:   Option<Vec<u8>>,
    pub last_edit_op_id:  Option<[u8; 32]>,
    pub last_edit_ts:     Option<u64>,
    pub last_edit_author: Option<[u8; 32]>,
    pub last_edit_blocks: Option<Vec<u8>>,
}

/// One synthesised feed event sourced from existing op tables.  The app
/// converts these into `FeedItem`s for rendering and live-event merging.
#[derive(Debug, Clone)]
pub struct FeedRow {
    pub event_id:      [u8; 32],
    pub kind:          FeedRowKind,
    pub interp_key:    String,
    pub body:          String,
    pub author_pk:     Option<[u8; 32]>,
    /// Set when `kind == Response`: the log_id of the parent interpretation.
    pub parent_log_id: Option<[u8; 32]>,
    /// Set when `kind == Affirm`: the log_id of the affirmed interpretation.
    pub target_log_id: Option<[u8; 32]>,
    pub ts:            u64,
    /// True iff `author_pk == me`.  Allows the app to filter own activity
    /// out of bell badging without re-querying.
    pub author_is_me:  bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedRowKind {
    Authored,
    Affirm,
    Response,
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod doc_ring_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn has_contributor_true_only_for_authors_still_in_the_ring() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let key = "natal:sun_trine_moon";
        let block = [1u8; 16];
        let alice = [0xAAu8; 32];
        let bob   = [0xBBu8; 32];

        assert!(!store.doc_has_contributor(key, &alice).await.unwrap());

        store.block_ring_push(key, &block, &alice, &[1u8; 32], 100, false).await.unwrap();
        assert!(store.doc_has_contributor(key, &alice).await.unwrap());
        assert!(!store.doc_has_contributor(key, &bob).await.unwrap());

        store.block_ring_push(key, &block, &bob, &[2u8; 32], 101, false).await.unwrap();
        // Both remain: the ring holds up to RING_SIZE distinct edits, and
        // two entries doesn't evict Alice's — a real bug this guards
        // against would be treating the ring as single-author.
        assert!(store.doc_has_contributor(key, &alice).await.unwrap());
        assert!(store.doc_has_contributor(key, &bob).await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn has_contributor_is_scoped_to_the_right_interp_key() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let alice = [0xAAu8; 32];
        store.block_ring_push("natal:sun_trine_moon", &[1u8; 16], &alice, &[1u8; 32], 100, false).await.unwrap();
        assert!(!store.doc_has_contributor("natal:mars_square_saturn", &alice).await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ai_generated_flag_round_trips_through_the_ring() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let key = "natal:sun_trine_moon";
        let block = [1u8; 16];
        let alice = [0xAAu8; 32];

        store.block_ring_push(key, &block, &alice, &[1u8; 32], 100, true).await.unwrap();
        let ring = store.block_ring_get(key, &block).await.unwrap();
        assert_eq!(ring.len(), 1);
        assert!(ring[0].3, "ai_generated should round-trip as true");
        assert!(store.doc_has_ai_generated_content(key).await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn doc_has_ai_generated_content_is_false_for_ordinary_edits() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let key = "natal:sun_trine_moon";
        store.block_ring_push(key, &[1u8; 16], &[0xAAu8; 32], &[1u8; 32], 100, false).await.unwrap();
        assert!(!store.doc_has_ai_generated_content(key).await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn doc_has_ai_generated_content_is_scoped_to_the_right_interp_key() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        store.block_ring_push("natal:sun_trine_moon", &[1u8; 16], &[0xAAu8; 32], &[1u8; 32], 100, true)
            .await.unwrap();
        assert!(!store.doc_has_ai_generated_content("natal:mars_square_saturn").await.unwrap());
    }
}

#[cfg(test)]
mod muted_peer_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn mute_unmute_roundtrip() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let bob = [0xBBu8; 32];

        assert!(store.muted_peers().await.unwrap().is_empty());

        store.mute_peer(&bob).await.unwrap();
        assert_eq!(store.muted_peers().await.unwrap(), vec![bob]);

        // Muting an already-muted peer is a no-op, not an error.
        store.mute_peer(&bob).await.unwrap();
        assert_eq!(store.muted_peers().await.unwrap(), vec![bob]);

        store.unmute_peer(&bob).await.unwrap();
        assert!(store.muted_peers().await.unwrap().is_empty());

        // Unmuting someone never muted is also a no-op, not an error.
        store.unmute_peer(&bob).await.unwrap();
    }
}

#[cfg(test)]
mod last_seen_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn records_and_overwrites_last_seen() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let bob = [0xBBu8; 32];

        assert!(store.all_last_seen().await.unwrap().is_empty());

        store.record_last_seen(&bob, 1000).await.unwrap();
        assert_eq!(store.all_last_seen().await.unwrap(), vec![(bob, 1000)]);

        // A later disconnect overwrites the earlier timestamp rather than
        // adding a second row.
        store.record_last_seen(&bob, 2000).await.unwrap();
        assert_eq!(store.all_last_seen().await.unwrap(), vec![(bob, 2000)]);
    }
}

#[cfg(test)]
mod feed_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn feed_read_roundtrip() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let id = [7u8; 32];
        assert!(!store.is_event_read(&id).await.unwrap());
        store.mark_event_read(&id).await.unwrap();
        assert!(store.is_event_read(&id).await.unwrap());
        store.mark_event_unread(&id).await.unwrap();
        assert!(!store.is_event_read(&id).await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bulk_mark_read_inserts_once() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let ids = vec![[1u8; 32], [2u8; 32], [3u8; 32]];
        let n = store.bulk_mark_read(&ids).await.unwrap();
        assert_eq!(n, 3);
        // Idempotent — second call inserts 0.
        let n2 = store.bulk_mark_read(&ids).await.unwrap();
        assert_eq!(n2, 0);
        for id in &ids {
            assert!(store.is_event_read(id).await.unwrap());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn feed_meta_roundtrip() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        assert_eq!(store.get_feed_meta("x").await.unwrap(), None);
        store.set_feed_meta("x", b"hello").await.unwrap();
        assert_eq!(store.get_feed_meta("x").await.unwrap().as_deref(), Some(&b"hello"[..]));
        store.set_feed_meta("x", b"world").await.unwrap();
        assert_eq!(store.get_feed_meta("x").await.unwrap().as_deref(), Some(&b"world"[..]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn doc_save_with_history_then_rollback_restores_prior() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let key = "natal:test";
        let snap_v1 = vec![1u8; 16];
        let snap_v2 = vec![2u8; 16];
        let rev_v1 = [0xAAu8; 32];
        let rev_v2 = [0xBBu8; 32];
        let edit_op = [9u8; 32];
        let author = [3u8; 32];
        let blocks = vec![0u8; 16]; // one BODY_BLOCK_ID
        // Seed an initial snapshot via doc_save (no history yet).
        store.doc_save(key, &snap_v1, &rev_v1).await.unwrap();
        // Apply edit v2 with prior=v1.
        store.doc_save_with_history(
            key, &snap_v2, &rev_v2, Some(&snap_v1),
            &edit_op, 1234, &author, &blocks,
        ).await.unwrap();
        // Ring push so rollback's ring pop has something to drop.
        store.block_ring_push(key, &[0u8; 16], &author, &edit_op, 1234, false)
            .await.unwrap();
        assert_eq!(store.doc_load(key).await.unwrap().unwrap(), snap_v2);
        let meta = store.doc_load_meta(key).await.unwrap().unwrap();
        assert_eq!(meta.last_edit_op_id, Some(edit_op));
        assert_eq!(meta.prior_snapshot.as_deref(), Some(snap_v1.as_slice()));
        // Roll back.
        let rolled = store.doc_rollback(key, &rev_v1, &[[0u8; 16]]).await.unwrap();
        assert!(rolled);
        assert_eq!(store.doc_load(key).await.unwrap().unwrap(), snap_v1);
        // Metadata cleared.
        let meta2 = store.doc_load_meta(key).await.unwrap().unwrap();
        assert!(meta2.last_edit_op_id.is_none());
        assert!(meta2.prior_snapshot.is_none());
        // Ring entry popped.
        let ring = store.block_ring_get(key, &[0u8; 16]).await.unwrap();
        assert!(ring.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn doc_rollback_noop_when_no_prior() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let key = "natal:noprior";
        store.doc_save(key, &[1u8; 8], &[0u8; 32]).await.unwrap();
        let rolled = store.doc_rollback(key, &[0u8; 32], &[]).await.unwrap();
        assert!(!rolled);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn targeting_unread_count_excludes_self_and_read() {
        let store = ZodiaStore::open_in_memory().await.unwrap();
        let me = [9u8; 32];
        let other = [1u8; 32];
        // Insert an interp authored by me with a stable canonical key string.
        let key = "natal:sun_trine_moon";
        let inserted = store.insert_from_op(key, "body", &me).await.unwrap();
        assert!(inserted);
        let log_id_bytes = derive_log_id(key, "body");
        // Affirmation from other → targets me.
        store.affirm(&log_id_bytes, &other).await.unwrap();
        // Affirmation from self → does NOT target me.
        store.affirm(&log_id_bytes, &me).await.unwrap();
        let n = store.feed_targeting_unread_count(&me).await.unwrap();
        assert_eq!(n, 1, "only the other-affirmation targets me");
        // Mark the targeting one read; count drops to 0.
        let ids = store.feed_targeting_unread_ids(&me).await.unwrap();
        assert_eq!(ids.len(), 1);
        store.bulk_mark_read(&ids).await.unwrap();
        let n2 = store.feed_targeting_unread_count(&me).await.unwrap();
        assert_eq!(n2, 0);
    }
}

// ── schema ────────────────────────────────────────────────────────────────────

const SCHEMA_STMTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS interpretations (
        log_id      BLOB    NOT NULL PRIMARY KEY,
        interp_key  TEXT    NOT NULL,
        interp_kind TEXT    NOT NULL,
        body        TEXT    NOT NULL,
        author_pk   BLOB,
        author_sig  BLOB,
        received_at INTEGER NOT NULL,
        is_baseline INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE INDEX IF NOT EXISTS idx_interp_key  ON interpretations(interp_key)",
    "CREATE INDEX IF NOT EXISTS idx_interp_kind ON interpretations(interp_kind)",
    "CREATE TABLE IF NOT EXISTS messages (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        peer_id   BLOB    NOT NULL,
        from_us   INTEGER NOT NULL,
        body      TEXT    NOT NULL,
        ts        INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_msg_peer ON messages(peer_id, ts)",
    "CREATE TABLE IF NOT EXISTS affirmations (
        log_id        BLOB    NOT NULL PRIMARY KEY,
        interp_log_id BLOB    NOT NULL,
        author_pk     BLOB    NOT NULL,
        created_at    INTEGER NOT NULL,
        UNIQUE(interp_log_id, author_pk)
    )",
    "CREATE INDEX IF NOT EXISTS idx_aff_interp ON affirmations(interp_log_id)",
    // Phase E: per-event read state for the activity feed.  Event ids are 32-byte
    // hashes — op hashes for pipeline events, deterministic blake3 digests for
    // synthetic events (transit enter/leave).
    "CREATE TABLE IF NOT EXISTS feed_read (
        event_id  BLOB    NOT NULL PRIMARY KEY,
        read_at   INTEGER NOT NULL
    )",
    // Phase E: durable feed-related local state (e.g. previous tick's in-orb
    // transit set so a restart doesn't re-emit every active transit).
    "CREATE TABLE IF NOT EXISTS feed_meta (
        key   TEXT NOT NULL PRIMARY KEY,
        value BLOB NOT NULL
    )",
    // Phase F-collab: per-key collaborative doc, persisted as a Loro
    // snapshot blob.  One row per interp_key; replaced wholesale on save.
    "CREATE TABLE IF NOT EXISTS interp_docs (
        interp_key       TEXT    NOT NULL PRIMARY KEY,
        loro_snapshot    BLOB    NOT NULL,
        snapshot_rev     BLOB    NOT NULL,
        updated_at       INTEGER NOT NULL,
        prior_snapshot   BLOB,
        last_edit_op_id  BLOB,
        last_edit_ts     INTEGER,
        last_edit_author BLOB,
        last_edit_blocks BLOB
    )",
    // Phase F-collab: author-veto ring per (interp_key, block_id).
    // Position 0 = oldest, RING_SIZE-1 = newest.  Updated by the
    // materialiser whenever a DocOp::Edit lands.
    "CREATE TABLE IF NOT EXISTS doc_block_authors (
        interp_key   TEXT NOT NULL,
        block_id     BLOB NOT NULL,
        position     INTEGER NOT NULL,
        author_pk    BLOB NOT NULL,
        edit_op_id   BLOB NOT NULL,
        edited_at    INTEGER NOT NULL,
        ai_generated INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (interp_key, block_id, position)
    )",
    "CREATE INDEX IF NOT EXISTS idx_doc_block_authors_by_author
        ON doc_block_authors(author_pk)",
    // Phase F-collab: affirmation targets a (interp_key, revision) instead
    // of a content-hash log_id.
    "CREATE TABLE IF NOT EXISTS doc_affirms (
        interp_key  TEXT NOT NULL,
        target_rev  BLOB NOT NULL,
        voter_pk    BLOB NOT NULL,
        affirmed_at INTEGER NOT NULL,
        PRIMARY KEY (interp_key, target_rev, voter_pk)
    )",
    // A peer's self-broadcast display name (InterpOp::SetDisplayName).
    // One row per peer, replaced wholesale — last-writer-wins by
    // `updated_at` (the op's Timestamp extension), enforced by the caller
    // (`set_peer_display_name_if_newer`) rather than the schema.  Purely an
    // untrusted display hint: a local nickname always takes precedence, see
    // `zodia_ops::InterpOp::SetDisplayName`'s doc comment.
    "CREATE TABLE IF NOT EXISTS peer_display_names (
        peer_pk     BLOB    NOT NULL PRIMARY KEY,
        name        TEXT    NOT NULL,
        updated_at  INTEGER NOT NULL
    )",
    // A muted peer's social activity (new readings, replies, hearts) is
    // still fully synced and stored — muting only suppresses the live
    // `StateEvent` feed notification, purely local, never synced itself
    // (same "local-only" reasoning as `peer_display_names`' nickname
    // override, just the opposite direction: hiding someone rather than
    // relabeling them).
    "CREATE TABLE IF NOT EXISTS muted_peers (
        peer_pk  BLOB    NOT NULL PRIMARY KEY,
        muted_at INTEGER NOT NULL
    )",
    // When a peer's direct channel last closed — lets the UI show "Last
    // seen 5m ago" instead of just a gray dot. Purely local, one row per
    // peer, replaced wholesale on each disconnect.
    "CREATE TABLE IF NOT EXISTS peer_last_seen (
        peer_pk BLOB    NOT NULL PRIMARY KEY,
        seen_at INTEGER NOT NULL
    )",
];

// ── internal helpers ──────────────────────────────────────────────────────────

fn derive_log_id(a: &str, b: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(a.as_bytes());
    hasher.update(b.as_bytes());
    *hasher.finalize().as_bytes()
}

fn kind_str(key: &InterpKey) -> &'static str {
    match key {
        InterpKey::Natal { .. }          => "natal",
        InterpKey::Synastry { .. }       => "synastry",
        InterpKey::SkyAspect { .. }      => "sky",
        InterpKey::Transit { .. }        => "transit",
        InterpKey::HouseTransit { .. }   => "house_transit",
        InterpKey::PlacementSign { .. }  => "placement_sign",
        InterpKey::PlacementHouse { .. } => "placement_house",
        InterpKey::PlacementAngle { .. } => "placement_angle",
    }
}

fn kind_from_key_str(key: &str) -> &str {
    key.split(':').next().unwrap_or("natal")
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
