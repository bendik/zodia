//! Read / write the 32-byte ed25519 identity seed at `identity.key` (mode 0600).

use std::path::Path;
use thiserror::Error;
use tracing::{debug, info};
use zodia_crypto::IdentityKeypair;

#[derive(Debug, Error)]
pub enum IdentityFileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity.key has wrong length (expected 32 bytes, got {0})")]
    BadLength(usize),
}

/// Load the seed from `path`, or generate and persist a fresh one.
pub fn load_or_generate(path: &Path) -> Result<IdentityKeypair, IdentityFileError> {
    if path.exists() {
        debug!(path = %path.display(), "loading existing identity seed");
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            return Err(IdentityFileError::BadLength(bytes.len()));
        }
        let seed: [u8; 32] = bytes.try_into().unwrap();
        Ok(IdentityKeypair::from_seed(seed))
    } else {
        info!(path = %path.display(), "generating new identity keypair");
        let keypair = IdentityKeypair::generate();
        write_seed(path, keypair.seed())?;
        Ok(keypair)
    }
}

fn write_seed(path: &Path, seed: [u8; 32]) -> Result<(), IdentityFileError> {
    use std::fs::OpenOptions;

    // Write with 0600 permissions so no other user can read the seed.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        use std::io::Write;
        file.write_all(&seed)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, &seed)?;
    }
    Ok(())
}
