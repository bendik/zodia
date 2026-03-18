//! Consent tier key material for Zodia.
//!
//! Each tier corresponds to a distinct cryptographic commitment:
//!
//! | Tier | Data disclosed        | Mechanism                        |
//! |------|-----------------------|----------------------------------|
//! |  0   | geohash[..5] + month  | ed25519 signed broadcast         |
//! |  1   | geohash[..9] + jdn    | X3DH over noise channel (mutual) |
//! |  2   | WebRTC offer/answer   | Tier-1 ratchet channel           |
//! |  3   | name / handle         | In-session plaintext, voluntary  |
//!
//! The `p2panda-encryption` dependency and Double Ratchet wiring will be
//! added when crate versions are pinned.

use serde::{Deserialize, Serialize};
use zodia_core::BirthData;

/// Long-term ed25519 identity keypair (p2panda identity).
#[derive(Debug)]
pub struct IdentityKeypair {
    pub public: [u8; 32],
    #[allow(dead_code)]
    secret: [u8; 32],
}

/// Ephemeral X25519 keypair — one per session, never stored.
#[derive(Debug)]
pub struct SessionKeypair {
    pub public: [u8; 32],
    #[allow(dead_code)]
    secret: [u8; 32],
}

/// Full consent state for a peer relationship.
///
/// Advances monotonically: 0 → 1 → 2 → 3.
/// Regression is not permitted — a peer who withdraws consent
/// must be removed entirely and a new state created on reconnect.
#[derive(Debug)]
pub struct ConsentState {
    pub tier: u8,
    pub identity: IdentityKeypair,
    pub session: Option<SessionKeypair>,
    pub peer_pubkey: Option<[u8; 32]>,
    // ratchet: Option<DoubleRatchetSession>  -- added with p2panda-encryption
}

#[derive(Debug, thiserror::Error)]
pub enum ConsentError {
    #[error("tier {requested} requires tier {required} first")]
    OutOfOrder { required: u8, requested: u8 },
    #[error("peer has not provided reciprocal data for tier {0}")]
    NotMutual(u8),
    #[error("cryptographic operation failed: {0}")]
    Crypto(String),
}

impl ConsentState {
    /// Advance to Tier 1 given the peer's Tier-1 blob.
    ///
    /// Performs X3DH key agreement and initialises the Double Ratchet session.
    /// Both sides must have sent their Tier-1 blob for this to succeed.
    pub fn advance_to_tier1(
        &mut self,
        peer_birth: BirthData,
        peer_prekey: [u8; 32],
    ) -> Result<Tier1Receipt, ConsentError> {
        if self.tier != 0 {
            return Err(ConsentError::OutOfOrder { required: 0, requested: 1 });
        }
        // TODO: X3DH over the established noise channel using self.session keypair
        // and peer_prekey, then initialise DoubleRatchetSession.
        self.peer_pubkey = Some(peer_prekey);
        self.tier = 1;
        Ok(Tier1Receipt { peer_birth })
    }

    /// Advance to Tier 2 — both parties have signalled readiness for AV.
    pub fn advance_to_tier2(&mut self) -> Result<(), ConsentError> {
        if self.tier < 1 {
            return Err(ConsentError::OutOfOrder { required: 1, requested: 2 });
        }
        self.tier = 2;
        Ok(())
    }
}

/// Returned by `advance_to_tier1` — carries the now-trusted peer birth data.
#[derive(Debug, Serialize, Deserialize)]
pub struct Tier1Receipt {
    pub peer_birth: BirthData,
}
