//! ECIES (Integrated Encryption Scheme) over X25519 + BLAKE3 + XChaCha20-Poly1305.
//!
//! Used for blind-relay message encryption: the sender encrypts to a stable
//! X25519 relay key derived from the recipient's identity, so a relay node
//! forwarding the ciphertext cannot read the content.
//!
//! Wire format: `ek_pub (32 B) || nonce (24 B) || ciphertext+tag (≥16 B)`

use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng as AeadOsRng},
    XChaCha20Poly1305,
};
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

use crate::ratchet::RatchetError;

/// KDF context string — changing this breaks all existing encrypted payloads.
const ECIES_CONTEXT: &str = "zodia ecies v1";

/// Encrypt `plaintext` to the holder of `dest_relay_pk`.
///
/// Generates a fresh ephemeral X25519 keypair for this message.
/// Output: `ek_pub (32 B) || nonce (24 B) || ciphertext+tag`.
pub fn ecies_encrypt(dest_relay_pk: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let ek = EphemeralSecret::random_from_rng(rand_core::OsRng);
    let ek_pub = PublicKey::from(&ek);
    let dest_pk = PublicKey::from(*dest_relay_pk);
    let shared = ek.diffie_hellman(&dest_pk);

    let key_bytes = derive_key(shared.as_bytes(), ek_pub.as_bytes(), dest_relay_pk);
    let cipher = XChaCha20Poly1305::new((&key_bytes).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut AeadOsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .expect("XChaCha20Poly1305 ECIES encryption is infallible for valid inputs");

    let mut out = Vec::with_capacity(32 + 24 + ciphertext.len());
    out.extend_from_slice(ek_pub.as_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out
}

/// Decrypt a payload produced by [`ecies_encrypt`].
///
/// `our_relay_sk` is the 32-byte scalar for the recipient's stable relay key
/// — obtainable via [`crate::IdentityKeypair::relay_secret_bytes`].
pub fn ecies_decrypt(our_relay_sk: &[u8; 32], payload: &[u8]) -> Result<Vec<u8>, RatchetError> {
    // Minimum: 32 (ek_pub) + 24 (nonce) + 16 (Poly1305 tag) = 72
    if payload.len() < 72 {
        return Err(RatchetError::Decrypt);
    }

    let ek_pub_bytes: [u8; 32] = payload[..32].try_into().unwrap();
    let nonce_bytes: [u8; 24] = payload[32..56].try_into().unwrap();
    let ciphertext = &payload[56..];

    let sk = StaticSecret::from(*our_relay_sk);
    let our_pk: [u8; 32] = PublicKey::from(&sk).to_bytes();
    let ek_pub = PublicKey::from(ek_pub_bytes);
    let shared = sk.diffie_hellman(&ek_pub);

    let key_bytes = derive_key(shared.as_bytes(), &ek_pub_bytes, &our_pk);
    let cipher = XChaCha20Poly1305::new((&key_bytes).into());
    cipher
        .decrypt((&nonce_bytes).into(), ciphertext)
        .map_err(|_| RatchetError::Decrypt)
}

fn derive_key(shared: &[u8; 32], ek_pub: &[u8; 32], dest_pk: &[u8; 32]) -> [u8; 32] {
    let mut kdf_input = Vec::with_capacity(96);
    kdf_input.extend_from_slice(shared);
    kdf_input.extend_from_slice(ek_pub);
    kdf_input.extend_from_slice(dest_pk);
    blake3::derive_key(ECIES_CONTEXT, &kdf_input)
}
