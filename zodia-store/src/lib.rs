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

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use zodia_core::InterpKey;

pub use seed::BaselineData;

pub mod seed;

// ── error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("seed parse error: {0}")]
    Seed(String),
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
        Ok(())
    }

    // ── interpretations ───────────────────────────────────────────────────────

    /// Insert a single interpretation.  Returns the generated log_id.
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

    /// Number of non-baseline interpretations in the store.
    pub fn community_count(&self) -> Result<u64, StoreError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM interpretations WHERE is_baseline = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// True if no interpretations exist at all (used to decide whether to seed).
    pub fn is_empty(&self) -> Result<bool, StoreError> {
        let n: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM interpretations", [], |r| r.get(0))?;
        Ok(n == 0)
    }

    // ── affirmations ──────────────────────────────────────────────────────────

    /// Record an affirmation.  Returns `Ok(true)` if newly inserted, `Ok(false)`
    /// if this author had already affirmed this interpretation.
    pub fn affirm(
        &self,
        interp_log_id: &[u8; 32],
        author_pk: &[u8; 32],
    ) -> Result<bool, StoreError> {
        // Derive a stable ID from the two byte arrays directly.
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

    // ── seeding ───────────────────────────────────────────────────────────────

    /// Populate the store from baseline data if it is currently empty.
    ///
    /// Safe to call on every startup — if any entry exists the call returns 0
    /// immediately without touching the DB.
    pub fn seed_if_empty(&self, data: &seed::BaselineData) -> Result<u32, StoreError> {
        if !self.is_empty()? {
            return Ok(0);
        }
        self.seed_unconditional(data)
    }

    /// Insert all baseline entries, skipping any whose log_id already exists.
    pub fn seed_unconditional(&self, data: &seed::BaselineData) -> Result<u32, StoreError> {
        let mut count = 0u32;
        for (sig, body) in &data.natal {
            self.insert_raw_sig(&format!("natal:{sig}"), "natal", body)?;
            count += 1;
        }
        for (sig, body) in &data.transit {
            self.insert_raw_sig(&format!("transit:{sig}"), "transit", body)?;
            count += 1;
        }
        for (sig, body) in &data.house_transit {
            self.insert_raw_sig(&format!("house_transit:{sig}"), "house_transit", body)?;
            count += 1;
        }
        Ok(count)
    }

    fn insert_raw_sig(&self, sig: &str, kind: &str, body: &str) -> Result<(), StoreError> {
        let log_id = derive_log_id(sig, body);
        let now = unix_secs();
        self.conn.execute(
            "INSERT OR IGNORE INTO interpretations
             (log_id, interp_key, interp_kind, body, author_pk, received_at, is_baseline)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, 1)",
            params![log_id.as_slice(), sig, kind, body, now as i64],
        )?;
        Ok(())
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
    received_at INTEGER NOT NULL,
    is_baseline INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_interp_key  ON interpretations(interp_key);
CREATE INDEX IF NOT EXISTS idx_interp_kind ON interpretations(interp_kind);

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
        InterpKey::Natal { .. }       => "natal",
        InterpKey::Synastry { .. }    => "synastry",
        InterpKey::Transit { .. }     => "transit",
        InterpKey::HouseTransit { .. } => "house_transit",
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
