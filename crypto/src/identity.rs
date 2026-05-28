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
