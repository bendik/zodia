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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use zodia_core::BirthData;

    fn test_birth() -> BirthData {
        // 2000-01-01 12:00 UT, geohash "u4pru" (Oslo area)
        BirthData::new(2451545.0, "u4pru")
    }

    fn test_key() -> SigningKey {
        // Deterministic seed for reproducible tests.
        SigningKey::from_bytes(&[0x42u8; 32])
    }

    fn test_key2() -> SigningKey {
        SigningKey::from_bytes(&[0xABu8; 32])
    }

    #[test]
    fn sign_and_verify_succeeds() {
        let key = test_key();
        let birth = test_birth();
        let blob = AnnounceBlob::sign(&birth, &key);
        blob.verify().expect("fresh blob should verify");
    }

    #[test]
    fn cbor_round_trip() {
        let key = test_key();
        let birth = test_birth();
        let blob = AnnounceBlob::sign(&birth, &key);
        let bytes = blob.to_cbor();
        let decoded = AnnounceBlob::from_cbor(&bytes).expect("round-trip decode failed");
        assert_eq!(blob.geohash_prefix, decoded.geohash_prefix);
        assert_eq!(blob.solar_month, decoded.solar_month);
        assert_eq!(blob.pubkey, decoded.pubkey);
        assert_eq!(blob.sig, decoded.sig);
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let key = test_key();
        let birth = test_birth();
        let mut blob = AnnounceBlob::sign(&birth, &key);
        // Flip a byte in the signature.
        blob.sig[0] ^= 0xFF;
        let err = blob.verify().expect_err("tampered sig should fail");
        assert!(matches!(err, AnnounceError::BadSignature));
    }

    #[test]
    fn tampered_pubkey_is_rejected() {
        let key = test_key();
        let birth = test_birth();
        let mut blob = AnnounceBlob::sign(&birth, &key);
        // Replace the pubkey with an all-zero key (not a valid curve point).
        blob.pubkey = [0u8; 32];
        let err = blob.verify().expect_err("bad pubkey should fail");
        assert!(matches!(err, AnnounceError::BadPublicKey | AnnounceError::BadSignature));
    }

    #[test]
    fn mismatched_key_signature_is_rejected() {
        let key1 = test_key();
        let key2 = test_key2();
        let birth = test_birth();
        // Sign with key1 but embed key2's pubkey.
        let mut blob = AnnounceBlob::sign(&birth, &key1);
        blob.pubkey = key2.verifying_key().to_bytes();
        let err = blob.verify().expect_err("wrong key should fail");
        assert!(matches!(err, AnnounceError::BadSignature));
    }

    #[test]
    fn truncated_cbor_is_rejected() {
        let key = test_key();
        let birth = test_birth();
        let blob = AnnounceBlob::sign(&birth, &key);
        let bytes = blob.to_cbor();
        // Truncate to half the bytes.
        let truncated = &bytes[..bytes.len() / 2];
        assert!(AnnounceBlob::from_cbor(truncated).is_err());
    }

    #[test]
    fn solar_month_field_matches_birth() {
        let key = test_key();
        // JDN 2451545.0 = 2000-01-01 — sun is in Capricorn (month 9 in 0-indexed)
        let birth = BirthData::new(2451545.0, "u4pru");
        let blob = AnnounceBlob::sign(&birth, &key);
        assert_eq!(blob.solar_month, zodia_core::solar_month(birth.jdn));
    }

    #[test]
    fn geohash_prefix_is_three_chars() {
        let key = test_key();
        let birth = BirthData::new(2451545.0, "u4pruydqqvj");
        let blob = AnnounceBlob::sign(&birth, &key);
        assert_eq!(blob.geohash_prefix.len(), 3);
        assert_eq!(&blob.geohash_prefix, &birth.geohash[..3]);
    }
}
