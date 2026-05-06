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

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, VerifyingKey};
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use thiserror::Error;
use zodia_core::InterpKey;

pub use seed::{BaselineData, BaselineStore};

pub mod seed;

// ── error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
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

pub struct ZodiaStore {
    conn: Connection,
}

impl ZodiaStore {
    /// Open (or create) the SQLite database at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// In-memory database — useful for tests and first-run seeding checks.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(SCHEMA)?;
        // Migrations for older DBs.
        let _ = self.conn.execute_batch(
            "ALTER TABLE interpretations ADD COLUMN author_sig BLOB;",
        );
        Ok(())
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
    pub fn insert_interpretation(
        &self,
        key: &InterpKey,
        body: &str,
        author_pk: Option<&[u8; 32]>,
        is_baseline: bool,
    ) -> Result<[u8; 32], StoreError> {
        let sig = key.to_sig();
        let log_id = derive_log_id(&sig, body);
        let now = unix_secs();
        self.conn.execute(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, received_at, is_baseline)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                log_id.as_slice(),
                sig,
                kind_str(key),
                body,
                author_pk.map(|b| b.as_slice()),
                now as i64,
                is_baseline as i32,
            ],
        )?;
        Ok(log_id)
    }

    /// Insert a locally authored community interpretation with its ed25519 signature.
    ///
    /// The caller must sign `ZodiaStore::signing_payload(key, body)` with the
    /// author's identity key before calling this.
    pub fn insert_signed(
        &self,
        key: &InterpKey,
        body: &str,
        author_pk: &[u8; 32],
        author_sig: &[u8; 64],
    ) -> Result<[u8; 32], StoreError> {
        let sig = key.to_sig();
        let log_id = derive_log_id(&sig, body);
        let now = unix_secs();
        self.conn.execute(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, author_sig, received_at, is_baseline)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                log_id.as_slice(),
                sig,
                kind_str(key),
                body,
                author_pk.as_slice(),
                author_sig.as_slice(),
                now as i64,
            ],
        )?;
        Ok(log_id)
    }

    /// Verify and insert a community interpretation received from a peer.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already present,
    /// `Err(StoreError::InvalidSignature)` if the signature does not verify.
    pub fn insert_received(
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
        let now = unix_secs();
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, author_sig, received_at, is_baseline)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                log_id.as_slice(),
                interp_key,
                kind,
                body,
                author_pk.as_slice(),
                author_sig.as_slice(),
                now as i64,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Collect top non-baseline, signed community entries for the given canonical
    /// key strings.  Returns at most `limit` rows, sorted by affirmation count.
    ///
    /// Only entries with a valid `author_sig` are included — unsigned baseline
    /// entries and legacy unsigned community entries are excluded.
    pub fn community_for_keys(
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
               AND interp_key IN ({placeholders})
             ORDER BY (
                 SELECT COUNT(*) FROM affirmations
                 WHERE interp_log_id = interpretations.log_id
             ) DESC
             LIMIT ?"
        );

        let mut params: Vec<Value> = key_sigs
            .iter()
            .map(|s| Value::Text(s.to_string()))
            .collect();
        params.push(Value::Integer(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                let pk_bytes: Vec<u8> = row.get(2)?;
                let sig_bytes: Vec<u8> = row.get(3)?;
                let mut author_pk = [0u8; 32];
                let mut author_sig = [0u8; 64];
                author_pk.copy_from_slice(&pk_bytes[..32.min(pk_bytes.len())]);
                author_sig.copy_from_slice(&sig_bytes[..64.min(sig_bytes.len())]);
                Ok(CommunityEntry {
                    interp_key: row.get(0)?,
                    body: row.get(1)?,
                    author_pk,
                    author_sig,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The single best interpretation for a key — community-contributed first,
    /// then sorted by affirmation count, with baseline as fallback.
    pub fn top_body(&self, key: &InterpKey) -> Result<Option<String>, StoreError> {
        let sig = key.to_sig();
        let body = self.conn.query_row(
            "SELECT body FROM interpretations
             WHERE interp_key = ?1
             ORDER BY is_baseline ASC,
                      (SELECT COUNT(*) FROM affirmations
                       WHERE interp_log_id = interpretations.log_id) DESC
             LIMIT 1",
            params![sig],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
        Ok(body)
    }

    /// All interpretations for a key, best-first.
    pub fn all_for_key(&self, key: &InterpKey) -> Result<Vec<InterpRow>, StoreError> {
        let sig = key.to_sig();
        let mut stmt = self.conn.prepare(
            "SELECT log_id, body, author_pk, received_at, is_baseline,
                    (SELECT COUNT(*) FROM affirmations WHERE interp_log_id = i.log_id) AS aff_count
             FROM interpretations i
             WHERE interp_key = ?1
             ORDER BY is_baseline ASC, aff_count DESC",
        )?;
        let rows = stmt
            .query_map(params![sig], |row| {
                let log_id_bytes: Vec<u8> = row.get(0)?;
                let mut log_id = [0u8; 32];
                log_id.copy_from_slice(&log_id_bytes[..32.min(log_id_bytes.len())]);
                let author_bytes: Option<Vec<u8>> = row.get(2)?;
                let author_pk = author_bytes.and_then(|b| {
                    if b.len() == 32 {
                        let mut a = [0u8; 32];
                        a.copy_from_slice(&b);
                        Some(a)
                    } else {
                        None
                    }
                });
                Ok(InterpRow {
                    log_id,
                    body: row.get(1)?,
                    author_pk,
                    received_at: row.get::<_, i64>(3)? as u64,
                    is_baseline: row.get::<_, i32>(4)? != 0,
                    affirmation_count: row.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Most recently received/authored community interpretations, newest first.
    pub fn recent_community_interps(&self, limit: usize) -> Result<Vec<RecentInterp>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT interp_key, body, received_at FROM interpretations
             WHERE is_baseline = 0 AND author_sig IS NOT NULL
             ORDER BY received_at DESC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(RecentInterp {
                    interp_key: row.get(0)?,
                    body: row.get(1)?,
                    received_at: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Number of non-baseline interpretations in the store.
    pub fn community_count(&self) -> Result<u64, StoreError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM interpretations WHERE is_baseline = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    // ── affirmations ──────────────────────────────────────────────────────────

    /// Record an affirmation.  Returns `Ok(true)` if newly inserted, `Ok(false)`
    /// if this author had already affirmed this interpretation.
    pub fn affirm(
        &self,
        interp_log_id: &[u8; 32],
        author_pk: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(interp_log_id);
        hasher.update(author_pk);
        let log_id: [u8; 32] = *hasher.finalize().as_bytes();
        let now = unix_secs();
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO affirmations (log_id, interp_log_id, author_pk, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                log_id.as_slice(),
                interp_log_id.as_slice(),
                author_pk.as_slice(),
                now as i64,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Affirmation count for one interpretation.
    pub fn affirmation_count(&self, log_id: &[u8; 32]) -> Result<u64, StoreError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM affirmations WHERE interp_log_id = ?1",
            params![log_id.as_slice()],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    // ── messages ──────────────────────────────────────────────────────────────

    /// Persist a single chat message.
    pub fn insert_message(
        &self,
        peer_id: &[u8; 32],
        from_us: bool,
        body: &str,
    ) -> Result<(), StoreError> {
        let ts = unix_ms();
        self.conn.execute(
            "INSERT INTO messages (peer_id, from_us, body, ts) VALUES (?1, ?2, ?3, ?4)",
            params![peer_id.as_slice(), from_us as i32, body, ts as i64],
        )?;
        Ok(())
    }

    /// Load all messages for a peer, oldest-first.
    pub fn messages_for_peer(
        &self,
        peer_id: &[u8; 32],
    ) -> Result<Vec<(bool, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_us, body FROM messages WHERE peer_id = ?1 ORDER BY ts ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![peer_id.as_slice()], |row| {
                let from_us: i32 = row.get(0)?;
                let body: String = row.get(1)?;
                Ok((from_us != 0, body))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── migration ─────────────────────────────────────────────────────────────

    /// Delete all legacy seeded baseline rows. Idempotent — safe every startup.
    pub fn scrub_baseline(&self) -> Result<u64, StoreError> {
        let n = self.conn.execute(
            "DELETE FROM interpretations WHERE is_baseline = 1",
            [],
        )?;
        Ok(n as u64)
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

// ── schema ────────────────────────────────────────────────────────────────────

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS interpretations (
    log_id      BLOB    NOT NULL PRIMARY KEY,
    interp_key  TEXT    NOT NULL,
    interp_kind TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    author_pk   BLOB,
    author_sig  BLOB,
    received_at INTEGER NOT NULL,
    is_baseline INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_interp_key  ON interpretations(interp_key);
CREATE INDEX IF NOT EXISTS idx_interp_kind ON interpretations(interp_kind);

CREATE TABLE IF NOT EXISTS messages (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    peer_id   BLOB    NOT NULL,
    from_us   INTEGER NOT NULL,
    body      TEXT    NOT NULL,
    ts        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_msg_peer ON messages(peer_id, ts);

CREATE TABLE IF NOT EXISTS affirmations (
    log_id        BLOB    NOT NULL PRIMARY KEY,
    interp_log_id BLOB    NOT NULL,
    author_pk     BLOB    NOT NULL,
    created_at    INTEGER NOT NULL,
    UNIQUE(interp_log_id, author_pk)
);
CREATE INDEX IF NOT EXISTS idx_aff_interp ON affirmations(interp_log_id);
";

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
