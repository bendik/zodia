//! XDG-style local persistent configuration for Zodia.
//!
//! Two pieces of state are persisted under `$XDG_DATA_HOME/zodia/`
//! (defaulting to `~/.local/share/zodia/`):
//!
//! | File            | Content                                  | Mode |
//! |-----------------|------------------------------------------|------|
//! | `identity.key`  | 32-byte ed25519 seed (raw binary)        | 0600 |
//! | `birth.cbor`    | CBOR-encoded `BirthData`                 | 0644 |
//!
//! `LocalConfig::load_or_create()` is the normal entry point.  It reads both
//! files if they exist, generating a fresh `IdentityKeypair` on first run.
//! Birth data must be set explicitly via `save_birth`.
//!
//! The identity seed is **not** encrypted at rest — full-disk encryption or a
//! secrets manager are out of scope for the MVP.  The 0600 mode is the only
//! mitigation at this layer.

mod identity_file;
mod birth_file;

pub use identity_file::IdentityFileError;
pub use birth_file::BirthFileError;

use std::path::PathBuf;
use thiserror::Error;
use tracing::{debug, info, instrument};
use zodia_core::BirthData;
use zodia_crypto::IdentityKeypair;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("identity file: {0}")]
    Identity(#[from] IdentityFileError),
    #[error("birth file: {0}")]
    Birth(#[from] BirthFileError),
    #[error("could not determine data directory (XDG_DATA_HOME / HOME unset)")]
    NoDataDir,
}

/// Resolved paths for the Zodia data directory.
#[derive(Debug, Clone)]
pub struct ZodiaDataDir(PathBuf);

impl ZodiaDataDir {
    /// Resolve `$XDG_DATA_HOME/zodia` or `~/.local/share/zodia`.
    pub fn resolve() -> Option<Self> {
        dirs::data_dir().map(|d| ZodiaDataDir(d.join("zodia")))
    }

    pub fn path(&self) -> &PathBuf {
        &self.0
    }

    fn identity_path(&self) -> PathBuf {
        self.0.join("identity.key")
    }

    fn birth_path(&self) -> PathBuf {
        self.0.join("birth.cbor")
    }
}

// ── public handle ─────────────────────────────────────────────────────────────

/// Loaded local configuration — identity keypair and optional birth data.
pub struct LocalConfig {
    pub identity: IdentityKeypair,
    pub birth: Option<BirthData>,
    dir: ZodiaDataDir,
}

impl LocalConfig {
    /// Load existing identity / birth data, or generate a fresh keypair on
    /// first run.
    ///
    /// The data directory is created if it does not exist.
    #[instrument(err)]
    pub fn load_or_create() -> Result<Self, ConfigError> {
        let dir = ZodiaDataDir::resolve().ok_or(ConfigError::NoDataDir)?;
        std::fs::create_dir_all(&dir.0)
            .map_err(IdentityFileError::Io)?;
        debug!(path = %dir.0.display(), "zodia data dir ready");

        let identity = identity_file::load_or_generate(&dir.identity_path())?;
        let birth = birth_file::load_if_present(&dir.birth_path())?;

        if birth.is_some() {
            info!("loaded identity + birth data");
        } else {
            info!("loaded identity (no birth data yet)");
        }

        Ok(LocalConfig { identity, birth, dir })
    }

    /// The resolved data directory — e.g. `~/.local/share/zodia/`.
    pub fn data_dir(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Persist `birth` to disk and update the in-memory copy.
    #[instrument(skip(self), err)]
    pub fn save_birth(&mut self, birth: BirthData) -> Result<(), ConfigError> {
        birth_file::save(&self.dir.birth_path(), &birth)?;
        info!(geohash = %birth.geohash, jdn = birth.jdn, "birth data saved");
        self.birth = Some(birth);
        Ok(())
    }
}
