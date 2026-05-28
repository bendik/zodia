//! Canonical operation enum for Zodia's wire format.
//!
//! Every valuable network event — authoring an interpretation, affirming
//! someone's, riffing on one — is encoded as an `InterpOp` and replicated
//! as the body of a p2panda `Operation<()>`.  The p2panda header carries
//! authentication (verifying key + signature); the body is just the
//! CBOR-encoded `InterpOp`.
//!
//! Phase A scope: `Author` is the only variant that actually flows yet;
//! `Affirm` and `RespondTo` are defined so the codec is stable from
//! day one and downstream code can pattern-match exhaustively.
//!
//! # Wire-format compatibility
//!
//! CBOR is used for forward-compatibility (extra map keys are ignored on
//! decode).  Variants are tagged by name via serde's default enum
//! representation; that representation is stable as long as variant names
//! are preserved.  Renames require a wire-format version bump.

use p2panda_core::Hash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── operation ─────────────────────────────────────────────────────────────────

/// A single Zodia operation, ready to be CBOR-encoded into a
/// `p2panda_core::Body`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterpOp {
    /// User authored a new interpretation for some `interp_key`.
    Author {
        /// Canonical key string, e.g. `"natal:jupiter_trine_venus"`.
        interp_key: String,
        /// The interpretation text.
        body: String,
    },

    /// User affirmed (♡'d) someone else's interpretation.
    ///
    /// `interp_op_id` is the BLAKE3 hash of the original `Author` op's
    /// header — the canonical operation identifier in p2panda.
    Affirm {
        interp_op_id: Hash,
    },

    /// User wrote a response that hangs off another peer's interpretation.
    /// The response is its own contribution; the `parent_op_id` link is
    /// what lets the pipeline assemble threads.
    RespondTo {
        parent_op_id: Hash,
        body: String,
    },
}

impl InterpOp {
    /// CBOR-encode the op into bytes suitable for a `p2panda_core::Body`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(self, &mut buf).expect("ciborium encode is infallible for owned data");
        buf
    }

    /// Decode bytes produced by `encode` back into an `InterpOp`.
    ///
    /// Returns `Err` for malformed CBOR or for CBOR that doesn't shape-match
    /// any current variant.  Forward-compatible: unknown extra fields inside
    /// a known variant are silently dropped.
    pub fn decode(bytes: &[u8]) -> Result<Self, OpCodecError> {
        ciborium::from_reader(bytes).map_err(|e| OpCodecError::Decode(e.to_string()))
    }
}

// ── error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum OpCodecError {
    #[error("CBOR decode error: {0}")]
    Decode(String),
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hash() -> Hash {
        // BLAKE3 of "test" — stable test fixture.
        let bytes = [
            0x48, 0x78, 0xca, 0x04, 0x25, 0xc7, 0x39, 0xfa,
            0x42, 0x7f, 0x7e, 0xda, 0x20, 0xfe, 0x84, 0x5f,
            0x6b, 0x2e, 0x46, 0xba, 0x5f, 0xe2, 0xa1, 0x4d,
            0xf5, 0xb1, 0xe3, 0x2f, 0x50, 0x55, 0x3d, 0x0a,
        ];
        Hash::from_bytes(bytes)
    }

    #[test]
    fn author_roundtrip() {
        let op = InterpOp::Author {
            interp_key: "natal:sun_trine_moon".into(),
            body: "The will is in agreement with the feelings.".into(),
        };
        let bytes = op.encode();
        let decoded = InterpOp::decode(&bytes).expect("decode roundtrip");
        assert_eq!(op, decoded);
    }

    #[test]
    fn affirm_roundtrip() {
        let op = InterpOp::Affirm { interp_op_id: sample_hash() };
        let bytes = op.encode();
        assert_eq!(op, InterpOp::decode(&bytes).unwrap());
    }

    #[test]
    fn respond_to_roundtrip() {
        let op = InterpOp::RespondTo {
            parent_op_id: sample_hash(),
            body: "Yes, and also the body knows it.".into(),
        };
        let bytes = op.encode();
        assert_eq!(op, InterpOp::decode(&bytes).unwrap());
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(InterpOp::decode(&[0xff; 8]).is_err());
        assert!(InterpOp::decode(&[]).is_err());
    }

    #[test]
    fn decode_is_forward_compatible_with_extra_map_keys() {
        // A future version might add a `tags: Vec<String>` field to Author.
        // Older clients must still decode the op cleanly, ignoring the unknown
        // field.  Constructed manually here as a CBOR map.
        use ciborium::value::Value;
        let mut author_map = Vec::<(Value, Value)>::new();
        author_map.push((Value::Text("interp_key".into()), Value::Text("natal:sun_sag".into())));
        author_map.push((Value::Text("body".into()),       Value::Text("Outward arrow.".into())));
        // Unknown field a future version might add:
        author_map.push((Value::Text("tags".into()),       Value::Array(vec![Value::Text("fire".into())])));
        let future_cbor = Value::Map(vec![
            (Value::Text("Author".into()), Value::Map(author_map)),
        ]);
        let mut buf = Vec::new();
        ciborium::into_writer(&future_cbor, &mut buf).unwrap();

        let decoded = InterpOp::decode(&buf).expect("forward-compat decode");
        match decoded {
            InterpOp::Author { interp_key, body } => {
                assert_eq!(interp_key, "natal:sun_sag");
                assert_eq!(body, "Outward arrow.");
            }
            other => panic!("expected Author, got {other:?}"),
        }
    }
}
