//! Read / write `BirthData` as CBOR at `birth.cbor`.

use std::path::Path;
use thiserror::Error;
use tracing::debug;
use zodia_core::BirthData;

#[derive(Debug, Error)]
pub enum BirthFileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    #[error("CBOR encode failed: {0}")]
    Encode(String),
}

/// Returns `None` if the file does not exist yet.
pub fn load_if_present(path: &Path) -> Result<Option<BirthData>, BirthFileError> {
    if !path.exists() {
        return Ok(None);
    }
    debug!(path = %path.display(), "loading birth data");
    let bytes = std::fs::read(path)?;
    let birth: BirthData = ciborium::from_reader(bytes.as_slice())
        .map_err(|e| BirthFileError::Decode(e.to_string()))?;
    Ok(Some(birth))
}

pub fn save(path: &Path, birth: &BirthData) -> Result<(), BirthFileError> {
    debug!(path = %path.display(), "saving birth data");
    let mut buf = Vec::new();
    ciborium::into_writer(birth, &mut buf)
        .map_err(|e| BirthFileError::Encode(e.to_string()))?;
    std::fs::write(path, &buf)?;
    Ok(())
}
