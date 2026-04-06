//! Anonymous announce blob — signing and verification.
//!
//! We use ed25519-dalek directly rather than p2panda-core's opaque `Signature`
//! wrapper so that verification from raw bytes is straightforward.  The key
//! bytes are identical to what p2panda-core uses internally (both back onto
//! the same ed25519 scalar), so the node identity is consistent end-to-end.

use crate::AnnounceBlob;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Signer, Verifier};
use serde::Serialize;
use thiserror::Error;
use zodia_core::{BirthData, solar_month};

#[derive(Debug, Error)]
pub enum AnnounceError {
    #[error("CBOR decode failed: {0}")]
    Decode(String),
    #[error("signature verification failed")]
    BadSignature,
    #[error("malformed public key")]
    BadPublicKey,
}

/// The exact bytes signed — serialised separately so we can reconstruct them
/// on the receiver side without the signature field.
#[derive(Serialize)]
struct AnnouncePayload<'a> {
    geohash_prefix: &'a str,
    solar_month: u8,
    pubkey: [u8; 32],
}

impl AnnounceBlob {
    /// Sign an announce blob using the identity signing key.
    pub fn sign(birth: &BirthData, signing_key: &SigningKey) -> Self {
        let prefix = &birth.geohash[..3.min(birth.geohash.len())];
        let month  = solar_month(birth.jdn);
        let pubkey = signing_key.verifying_key().to_bytes();

        let payload = AnnouncePayload { geohash_prefix: prefix, solar_month: month, pubkey };
        let sig = signing_key.sign(&cbor_encode(&payload));

        AnnounceBlob {
            geohash_prefix: prefix.to_string(),
            solar_month: month,
            pubkey,
            sig: sig.to_bytes().to_vec(),
        }
    }

    /// Verify the blob's ed25519 signature.  Returns an error if the key or
    /// signature bytes are malformed, or if the signature doesn't match.
    pub fn verify(&self) -> Result<(), AnnounceError> {
        let vk = VerifyingKey::from_bytes(&self.pubkey)
            .map_err(|_| AnnounceError::BadPublicKey)?;
        let Ok(sig_bytes): Result<[u8; 64], _> = self.sig.as_slice().try_into() else {
            return Err(AnnounceError::BadSignature);
        };
        let sig = Signature::from_bytes(&sig_bytes);
        let payload = AnnouncePayload {
            geohash_prefix: &self.geohash_prefix,
            solar_month: self.solar_month,
            pubkey: self.pubkey,
        };
        vk.verify(&cbor_encode(&payload), &sig)
            .map_err(|_| AnnounceError::BadSignature)
    }

    /// Serialize to CBOR bytes for gossip broadcast.
    pub fn to_cbor(&self) -> Vec<u8> {
        cbor_encode(self)
    }

    /// Deserialize from CBOR bytes received over gossip.
    pub fn from_cbor(bytes: &[u8]) -> Result<Self, AnnounceError> {
        ciborium::from_reader(bytes)
            .map_err(|e| AnnounceError::Decode(e.to_string()))
    }
}

fn cbor_encode<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .expect("CBOR encoding is infallible for in-memory writes");
    buf
}
