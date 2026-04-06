//! X3DH-style key agreement for the mutual consent exchange.
//!
//! Both parties include a static X25519 prekey and a fresh ephemeral key in
//! their ConsentBlob.  After the mutual exchange, each side performs three DH
//! operations and feeds the combined output through BLAKE3's KDF to produce
//! the shared 32-byte session key.
//!
//! DH operations (symmetric: DH(a,B) = DH(b,A)):
//!   dh1 = DH(my_ek, peer_ek)   — ephemeral-to-ephemeral (forward secrecy)
//!   dh2 = DH(my_ek, peer_pk)   — my ephemeral × peer prekey (authenticity)
//!   dh3 = DH(my_pk, peer_ek)   — my prekey × peer ephemeral (authenticity)
//!   sk  = BLAKE3-KDF(dh1 ‖ dh2 ‖ dh3)
//!
//! The session key then seeds the `SessionRatchet` for all subsequent
//! peer messages (voice signaling, identity reveal).

use x25519_dalek::{PublicKey as X25519Pub, ReusableSecret, StaticSecret};
use rand_core::OsRng;

/// The X25519 key material included in a `ConsentBlob`.
pub struct LocalPrekeys {
    pub prekey_secret: StaticSecret,
    pub prekey_public: [u8; 32],
    pub ephemeral_secret: ReusableSecret,
    pub ephemeral_public: [u8; 32],
}

impl LocalPrekeys {
    /// Generate a fresh set of X25519 prekeys for a consent exchange.
    pub fn generate() -> Self {
        let prekey_secret = StaticSecret::random_from_rng(OsRng);
        let prekey_public = X25519Pub::from(&prekey_secret).to_bytes();
        let ephemeral_secret = ReusableSecret::random_from_rng(OsRng);
        let ephemeral_public = X25519Pub::from(&ephemeral_secret).to_bytes();
        Self { prekey_secret, prekey_public, ephemeral_secret, ephemeral_public }
    }
}

impl std::fmt::Debug for LocalPrekeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secrets intentionally omitted from debug output.
        f.debug_struct("LocalPrekeys")
            .field("prekey_public",    &self.prekey_public)
            .field("ephemeral_public", &self.ephemeral_public)
            .finish_non_exhaustive()
    }
}

/// Peer's public prekey material received in their `ConsentBlob`.
#[derive(Debug, Clone)]
pub struct PeerPrekeys {
    pub prekey: [u8; 32],
    pub ephemeral: [u8; 32],
}

/// Derive the shared 32-byte session key from our local prekeys and the
/// peer's public prekeys.  Both peers arrive at the same value.
///
/// The BLAKE3 context string domain-separates this key from all others
/// derived in the Zodia protocol.  The string is a stable wire constant
/// and must not be changed once deployed.
pub fn derive_session_key(local: &LocalPrekeys, peer: &PeerPrekeys) -> [u8; 32] {
    let peer_ek = X25519Pub::from(peer.ephemeral);
    let peer_pk = X25519Pub::from(peer.prekey);

    let dh1 = local.ephemeral_secret.diffie_hellman(&peer_ek);
    let dh2 = local.ephemeral_secret.diffie_hellman(&peer_pk);
    let dh3 = local.prekey_secret.diffie_hellman(&peer_ek);

    let mut ikm = [0u8; 96];
    ikm[..32].copy_from_slice(dh1.as_bytes());
    ikm[32..64].copy_from_slice(dh2.as_bytes());
    ikm[64..96].copy_from_slice(dh3.as_bytes());

    // Wire constant — do not rename; changing this breaks sessions with existing peers.
    blake3::derive_key("zodia.net 2024 tier-1 session key v1", &ikm)
}
