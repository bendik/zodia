//! Consent tier key material for Zodia.
//!
//! The state machine advances monotonically; regression requires a fresh state.
//!
//! | Level | Data disclosed        | Mechanism                              |
//! |-------|-----------------------|----------------------------------------|
//! |  0    | geohash[..5] + month  | ed25519 signed gossip broadcast        |
//! |  1    | geohash[..9] + JDN    | X3DH over iroh QUIC (mutual consent)   |
//! |  2    | WebRTC offer/answer   | SessionRatchet on the consent channel  |
//! |  3    | name / handle         | SessionRatchet, in-session voluntary   |

pub mod ecies;
pub mod handshake;
pub mod identity;
pub mod ratchet;

pub use ecies::{ecies_decrypt, ecies_encrypt};
pub use handshake::{LocalPrekeys, PeerPrekeys, derive_session_key};
pub use identity::{IdentityKeypair, PublicIdentity};
pub use ratchet::{RatchetError, SessionRatchet};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, instrument};
use zodia_core::BirthData;

// ── error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ConsentError {
    #[error("consent level {requested} requires level {required} first")]
    OutOfOrder { required: u8, requested: u8 },
    #[error("peer has not sent their level {0} data yet")]
    NotMutual(u8),
    #[error("cryptographic error: {0}")]
    Crypto(#[from] RatchetError),
}

// ── consent state ─────────────────────────────────────────────────────────────

/// Full consent and key state for one peer relationship.
///
/// `ConsentState` is NOT `Clone` — the ratchet keys must live in exactly one
/// place.  The application holds one per peer it has consented with.
#[derive(Debug)]
pub struct ConsentState {
    pub level: u8,

    // identity — never changes
    pub identity: IdentityKeypair,

    // consent exchange key material
    prekeys: Option<LocalPrekeys>,
    pub peer_identity: Option<PublicIdentity>,
    pub peer_birth: Option<BirthData>,

    // active session (level ≥ 1)
    ratchet: Option<SessionRatchet>,
}

impl ConsentState {
    /// Create a fresh level-0 state for interacting with a new peer.
    pub fn new(identity: IdentityKeypair) -> Self {
        Self {
            level: 0,
            identity,
            prekeys: Some(LocalPrekeys::generate()),
            peer_identity: None,
            peer_birth: None,
            ratchet: None,
        }
    }

    /// The prekeys to include in our outgoing `ConsentBlob`.
    ///
    /// Panics if called after the prekeys have been consumed by `accept_consent`.
    pub fn prekey_bundle(&self) -> (/* prekey */ [u8; 32], /* ephemeral */ [u8; 32]) {
        let pk = self.prekeys.as_ref().expect("prekeys not yet consumed");
        (pk.prekey_public, pk.ephemeral_public)
    }

    /// Accept a completed mutual consent exchange and advance to level 1.
    ///
    /// Consumes the local prekeys to derive the session key via X3DH, then
    /// initialises the `SessionRatchet`.  The caller passes `initiator = true`
    /// if they opened the connection, `false` if they accepted it — this
    /// determines the chain orientation so send/receive keys never collide.
    #[instrument(skip(self, peer_blob_prekey, peer_blob_ephemeral, peer_identity_bytes),
                 fields(initiator))]
    pub fn accept_consent(
        &mut self,
        peer_blob_prekey: [u8; 32],
        peer_blob_ephemeral: [u8; 32],
        peer_birth: BirthData,
        peer_identity_bytes: [u8; 32],
        initiator: bool,
    ) -> Result<(), ConsentError> {
        if self.level != 0 {
            return Err(ConsentError::OutOfOrder { required: 0, requested: 1 });
        }

        let local = self.prekeys.take()
            .expect("prekeys already consumed — state machine bug");
        let peer = PeerPrekeys { prekey: peer_blob_prekey, ephemeral: peer_blob_ephemeral };

        let session_key = derive_session_key(&local, &peer);
        self.ratchet = Some(SessionRatchet::new(session_key, initiator));
        self.peer_birth = Some(peer_birth);
        self.peer_identity = Some(PublicIdentity { bytes: peer_identity_bytes });
        self.level = 1;
        info!("consent accepted — session ratchet initialised");
        Ok(())
    }

    /// Advance to level 2 — both sides have signalled readiness for voice/AV.
    #[instrument(skip(self))]
    pub fn accept_voice(&mut self) -> Result<(), ConsentError> {
        if self.level < 1 {
            return Err(ConsentError::OutOfOrder { required: 1, requested: 2 });
        }
        self.level = 2;
        info!("consent level 2 — voice/AV ready");
        Ok(())
    }

    // ── ratcheted messaging ───────────────────────────────────────────────────

    /// Encrypt `plaintext` for the peer using the current ratchet position.
    /// Returns the encrypted frame `[ nonce | ciphertext+tag ]`.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, ConsentError> {
        debug!(bytes = plaintext.len(), "encrypting message");
        self.ratchet
            .as_mut()
            .ok_or(ConsentError::OutOfOrder { required: 1, requested: 1 })?
            .encrypt(plaintext)
            .map_err(ConsentError::Crypto)
    }

    /// Decrypt a frame from the peer.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>, ConsentError> {
        debug!(bytes = frame.len(), "decrypting message");
        self.ratchet
            .as_mut()
            .ok_or(ConsentError::OutOfOrder { required: 1, requested: 1 })?
            .decrypt(frame)
            .map_err(ConsentError::Crypto)
    }
}

// ── receipt ───────────────────────────────────────────────────────────────────

/// Returned by a successful consent acceptance — the now-trusted exact birth data.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConsentReceipt {
    pub peer_birth: BirthData,
}
