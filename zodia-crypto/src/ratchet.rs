//! Symmetric message ratchet — forward-secret encryption for tier messages.
//!
//! Seeded from the X3DH shared session key, this ratchet provides:
//!   - A new message key for every send/receive (KDF chain advance)
//!   - XChaCha20-Poly1305 AEAD encryption with a per-message nonce
//!   - Independent send and receive counters per side
//!
//! This is a "KDF ratchet" (symmetric-key ratchet), not a full Double Ratchet
//! with DH ratchet steps.  That means it provides forward secrecy for past
//! messages (chain keys can't be reversed) but not break-in recovery.  The
//! full Double Ratchet (DH ratchet steps + KDF chain) is the planned upgrade
//! via `p2panda-encryption`'s `message_scheme` once the constructor API is
//! confirmed.
//!
//! Message format: [ nonce (24 bytes) | ciphertext+tag ]

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand_core::{OsRng, RngCore};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RatchetError {
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed — wrong key or corrupted ciphertext")]
    Decrypt,
    #[error("message too short to contain a nonce")]
    TooShort,
}

/// Ratchet state for one direction of a peer session.
///
/// Each side holds two of these: one for sending, one for receiving.
/// Both are seeded from the same `session_key` but advance independently.
#[derive(Debug)]
pub struct RatchetChain {
    chain_key: [u8; 32],
    counter: u64,
}

impl RatchetChain {
    /// Initialise a new chain from the session key.
    ///
    /// Use distinct context strings for the send and receive chains so they
    /// produce different key streams from the same seed.
    pub fn new(session_key: [u8; 32], context: &'static str) -> Self {
        let chain_key = blake3::derive_key(context, &session_key);
        Self { chain_key, counter: 0 }
    }

    /// Derive the next message key and advance the chain.
    ///
    /// After this call the old chain key is replaced — past message keys
    /// cannot be re-derived (forward secrecy).
    fn advance(&mut self) -> [u8; 32] {
        let mut input = [0u8; 40];
        input[..32].copy_from_slice(&self.chain_key);
        input[32..40].copy_from_slice(&self.counter.to_le_bytes());

        let message_key = blake3::derive_key("zodia ratchet message key", &input);
        self.chain_key  = blake3::derive_key("zodia ratchet chain advance", &self.chain_key);
        self.counter += 1;
        message_key
    }

    /// Encrypt `plaintext`, returning `[ nonce (24B) | ciphertext+tag ]`.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, RatchetError> {
        let message_key = self.advance();
        let cipher = XChaCha20Poly1305::new((&message_key).into());
        // Random nonce — 192 bits means collision probability is negligible.
        let mut nonce_bytes = [0u8; 24];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from(nonce_bytes);
        let ciphertext = cipher.encrypt(&nonce, plaintext)
            .map_err(|_| RatchetError::Encrypt)?;
        let mut out = Vec::with_capacity(24 + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt a frame produced by the peer's `encrypt`.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>, RatchetError> {
        if frame.len() < 24 {
            return Err(RatchetError::TooShort);
        }
        let (nonce_bytes, ciphertext) = frame.split_at(24);
        let nonce = XNonce::from_slice(nonce_bytes);
        let message_key = self.advance();
        let cipher = XChaCha20Poly1305::new((&message_key).into());
        cipher.decrypt(nonce, ciphertext)
            .map_err(|_| RatchetError::Decrypt)
    }
}

/// Paired send/receive ratchet chains for one peer session.
#[derive(Debug)]
pub struct SessionRatchet {
    send: RatchetChain,
    recv: RatchetChain,
}

impl SessionRatchet {
    /// Create from the X3DH-derived `session_key`.
    ///
    /// The initiator and responder use the same key but MUST call this with
    /// `initiator = true` and `initiator = false` respectively so their send
    /// and receive chains are swapped into the correct orientation.
    pub fn new(session_key: [u8; 32], initiator: bool) -> Self {
        let (send_ctx, recv_ctx) = if initiator {
            ("zodia ratchet initiator send chain", "zodia ratchet initiator recv chain")
        } else {
            ("zodia ratchet responder send chain", "zodia ratchet responder recv chain")
        };
        Self {
            send: RatchetChain::new(session_key, send_ctx),
            recv: RatchetChain::new(session_key, recv_ctx),
        }
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, RatchetError> {
        self.send.encrypt(plaintext)
    }

    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>, RatchetError> {
        self.recv.decrypt(frame)
    }
}
