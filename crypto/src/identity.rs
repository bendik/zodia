//! Long-term ed25519 identity keypair.
//!
//! These bytes serve triple duty:
//!   1. Signing Tier-0 announce blobs (zodia-net/announce.rs)
//!   2. p2panda operation signing (p2panda-core::PrivateKey)
//!   3. iroh transport identity (NodeId = public key bytes)
//!
//! We own the scalar in ed25519-dalek; conversions to the p2panda and iroh
//! types are derived from the same 32-byte seed.

use ed25519_dalek::{SigningKey, VerifyingKey, Signer};
use p2panda_core::SigningKey as PandaSigningKey;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

/// ed25519 identity keypair for a Zodia peer.
#[derive(Debug)]
pub struct IdentityKeypair {
    inner: SigningKey,
}

impl IdentityKeypair {
    /// Generate a fresh keypair using the OS CSPRNG.
    pub fn generate() -> Self {
        Self { inner: SigningKey::generate(&mut OsRng) }
    }

    /// Restore from a 32-byte seed (e.g. loaded from secure storage).
    pub fn from_seed(bytes: [u8; 32]) -> Self {
        Self { inner: SigningKey::from_bytes(&bytes) }
    }

    /// The 32-byte seed — store this in secure local storage.
    pub fn seed(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// The 32-byte ed25519 public key — this is the peer's on-wire identity.
    pub fn public_key(&self) -> [u8; 32] {
        self.inner.verifying_key().to_bytes()
    }

    /// Sign an arbitrary message.  Used for announce blobs.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.inner.sign(msg).to_bytes()
    }

    /// The underlying ed25519-dalek `SigningKey` — needed by zodia-net for
    /// `AnnounceBlob::sign` and for constructing the `NetworkConfig`.
    pub fn signing_key(&self) -> &SigningKey {
        &self.inner
    }

    /// Derive the p2panda-core `SigningKey` for operation signing.
    /// Both types are backed by the same ed25519 scalar.
    pub fn to_panda_key(&self) -> PandaSigningKey {
        PandaSigningKey::from_bytes(self.inner.as_bytes())
    }

    /// The stable X25519 public key used for ECIES relay encryption.
    ///
    /// Deterministically derived from the identity seed via BLAKE3.
    /// Senders encrypt relay payloads to this key; only this peer can decrypt.
    pub fn relay_public_key(&self) -> [u8; 32] {
        let sk = X25519Secret::from(self.relay_secret_bytes());
        X25519PublicKey::from(&sk).to_bytes()
    }

    /// The 32-byte scalar for the relay X25519 key.
    ///
    /// Pass to [`crate::ecies_decrypt`] to decrypt relay payloads addressed to us.
    pub fn relay_secret_bytes(&self) -> [u8; 32] {
        blake3::derive_key("zodia relay key v1", &self.inner.to_bytes())
    }
}

/// A verifying key paired with its raw bytes — avoids re-deriving for serde.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub bytes: [u8; 32],
}

impl PublicIdentity {
    pub fn from_keypair(kp: &IdentityKeypair) -> Self {
        Self { bytes: kp.public_key() }
    }

    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(&self.bytes) else { return false };
        let sig = ed25519_dalek::Signature::from_bytes(sig);
        vk.verify_strict(msg, &sig).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_roundtrip_preserves_public_key() {
        let original = IdentityKeypair::generate();
        let restored = IdentityKeypair::from_seed(original.seed());
        assert_eq!(original.public_key(), restored.public_key());
    }

    #[test]
    fn distinct_identities_have_distinct_public_keys() {
        let a = IdentityKeypair::generate();
        let b = IdentityKeypair::generate();
        assert_ne!(a.public_key(), b.public_key());
    }

    /// `to_panda_key` claims to be "the same ed25519 scalar" as the
    /// zodia-level identity — pin that the p2panda key's public bytes
    /// actually match, since a mismatch here would mean p2panda operations
    /// authenticate as a different identity than the app believes it is.
    #[test]
    fn to_panda_key_derives_the_same_public_key() {
        let identity = IdentityKeypair::generate();
        let panda_key = identity.to_panda_key();
        assert_eq!(
            panda_key.verifying_key().as_bytes(),
            &identity.public_key(),
        );
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let identity = IdentityKeypair::generate();
        let public   = PublicIdentity::from_keypair(&identity);
        let msg      = b"zodia consent handshake";

        let sig = identity.sign(msg);
        assert!(public.verify(msg, &sig));
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let identity = IdentityKeypair::generate();
        let public   = PublicIdentity::from_keypair(&identity);
        let sig      = identity.sign(b"original message");

        assert!(!public.verify(b"tampered message", &sig));
    }

    #[test]
    fn verify_rejects_signature_from_a_different_identity() {
        let a      = IdentityKeypair::generate();
        let b      = IdentityKeypair::generate();
        let public_b = PublicIdentity::from_keypair(&b);
        let sig_by_a  = a.sign(b"shared message");

        assert!(!public_b.verify(b"shared message", &sig_by_a));
    }

    /// `relay_public_key` is derived deterministically from the seed alone
    /// (no randomness) — pin that, since senders rely on recomputing the
    /// same key from a peer's announced identity to encrypt relay payloads.
    #[test]
    fn relay_public_key_is_deterministic_for_the_same_seed() {
        let seed = IdentityKeypair::generate().seed();
        let a = IdentityKeypair::from_seed(seed);
        let b = IdentityKeypair::from_seed(seed);
        assert_eq!(a.relay_public_key(), b.relay_public_key());
    }

    #[test]
    fn distinct_identities_have_distinct_relay_keys() {
        let a = IdentityKeypair::generate();
        let b = IdentityKeypair::generate();
        assert_ne!(a.relay_public_key(), b.relay_public_key());
    }
}
