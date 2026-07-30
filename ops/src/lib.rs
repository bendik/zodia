//! Canonical operation enum for Zodia's wire format.
//!
//! Every valuable network event — authoring an interpretation, affirming
//! someone's, riffing on one — is encoded as an `InterpOp` and replicated
//! as the body of a p2panda `Operation<OpExtensions>`.  The p2panda header
//! carries authentication (verifying key + signature) and, via
//! `OpExtensions`, the operation's timestamp (p2panda 0.7 dropped
//! `Header::timestamp` as a built-in field — extensions are now the
//! sanctioned place for per-operation metadata like this, per
//! `p2panda-core`'s own `extensions` example). The body is just the
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

use p2panda_core::{Extension, Hash, Header, Timestamp};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── header extensions ───────────────────────────────────────────────────────

/// Per-operation metadata carried in the p2panda `Header`'s extensions slot.
///
/// p2panda 0.7 removed `Header::timestamp` as a built-in field; this is the
/// replacement, following the pattern from `p2panda-core`'s own `extensions`
/// example (a small `Extension<T>`-implementing struct rather than a custom
/// side-channel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpExtensions {
    pub timestamp: Timestamp,
}

impl Extension<Timestamp> for OpExtensions {
    fn extract(header: &Header<Self>) -> Option<Timestamp> {
        Some(header.extensions.timestamp)
    }
}

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

    /// User affirmed (♡'d) an interpretation.
    ///
    /// `target_log_id` is `BLAKE3(interp_key || body)` — the Zodia content
    /// hash, matching the `log_id` column in `zodia-store::interpretations`.
    /// Targeting content rather than the specific authoring lets duplicate-
    /// content rows from different peers collapse into one community row
    /// with a combined affirmation count.  Sybil resistance is enforced
    /// downstream as (target_log_id, voter_pk) uniqueness.
    Affirm {
        target_log_id: Hash,
    },

    /// User wrote a response that hangs off another interpretation.
    /// The response is its own contribution; the `parent_log_id` link
    /// (`BLAKE3(interp_key || body)` of the parent) is what lets the
    /// pipeline assemble threads.  Same content-hash targeting model
    /// as `Affirm` — collapses duplicate-content parents and works
    /// regardless of which peer authored the parent.
    RespondTo {
        parent_log_id: Hash,
        body:          String,
    },

    /// Revoke a previously authored interpretation.  Only honoured at the
    /// materialisation layer when the revoker's verifying key matches the
    /// original author of `target_log_id`.  Peers tombstone the row in
    /// their stores; subsequent affirmations/responses against the row
    /// remain in storage but the parent is filtered from UI surfaces.
    ///
    /// Note: revoke is a propagation request, not a guarantee — peers who
    /// have already exported the content can still retain it.  The op name
    /// captures the intent ("to the extent of my capability").
    Revoke {
        target_log_id: Hash,
    },
}

// ── DocOp (Phase F-collab) ────────────────────────────────────────────────────

/// Collaborative-document operations.  One sibling of `InterpOp`: while the
/// legacy `InterpOp` variants treat the community body as a set of
/// competing whole interpretations, `DocOp` treats each `interp_key` as a
/// single converging text doc with author-veto-ring semantics.
///
/// New writes from 0.9+ use `DocOp` exclusively; `InterpOp::{Author,
/// RespondTo, Affirm}` remain in the wire format only for backwards-compat
/// reads of pre-0.9 ops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocOp {
    /// One CRDT update against a key's doc.  `base_rev` is the doc revision
    /// the editor's local state was at when the edit was authored;
    /// receivers use it for ordering hints.  `crdt_update` carries the
    /// CRDT-internal wire format (Loro update bytes).  `affected_blocks`
    /// is the set of block ids the edit touched — used by the materialiser
    /// to maintain the author ring.
    Edit {
        interp_key:      String,
        base_rev:        Hash,
        crdt_update:     Vec<u8>,
        affected_blocks: Vec<[u8; 16]>,
    },
    /// Veto a specific `Edit` op.  Honoured by receivers iff:
    ///   1. the revoker's pubkey sits in the ring of at least one of the
    ///      affected blocks at materialisation time,
    ///   2. the veto's op timestamp is within `VETO_WINDOW_DAYS` of the
    ///      target edit's op timestamp,
    ///   3. the target edit is still the *most recent* edit on the
    ///      affected blocks (a later edit invalidates stale vetoes).
    Veto {
        interp_key:        String,
        target_edit_op_id: Hash,
    },
    /// Affirm a specific revision of a key's doc.  Replaces the legacy
    /// "affirm one of many interpretations" model — the target is a
    /// doc-revision hash produced by `zodia_doc::InterpDoc::current_rev`.
    AffirmRev {
        interp_key: String,
        target_rev: [u8; 32],
    },
    /// Presence heartbeat: this peer is editing `interp_key` right now (or
    /// has stopped).  Lightweight; not stored in the long-term store —
    /// downstream renders "active editor" indicators based on recent
    /// presence ops alone.
    EditorPresence {
        interp_key: String,
        joined:     bool,
    },
}

impl DocOp {
    /// The key this op targets. Every variant carries one — used to route
    /// publishes to the per-key log (see `log_id_for_key`).
    pub fn interp_key(&self) -> &str {
        match self {
            DocOp::Edit { interp_key, .. }
            | DocOp::Veto { interp_key, .. }
            | DocOp::AffirmRev { interp_key, .. }
            | DocOp::EditorPresence { interp_key, .. } => interp_key,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::into_writer(self, &mut buf).expect("ciborium encode infallible for owned data");
        buf
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, OpCodecError> {
        ciborium::from_reader(bytes).map_err(|e| OpCodecError::Decode(e.to_string()))
    }
}

#[cfg(test)]
mod doc_op_tests {
    use super::*;

    fn sample_hash() -> Hash {
        Hash::from_bytes([7u8; 32])
    }

    #[test]
    fn interp_key_accessor_covers_every_variant() {
        assert_eq!(DocOp::Edit {
            interp_key: "natal:sun_trine_moon".into(), base_rev: sample_hash(),
            crdt_update: vec![], affected_blocks: vec![],
        }.interp_key(), "natal:sun_trine_moon");
        assert_eq!(DocOp::Veto {
            interp_key: "natal:x".into(), target_edit_op_id: sample_hash(),
        }.interp_key(), "natal:x");
        assert_eq!(DocOp::AffirmRev {
            interp_key: "natal:y".into(), target_rev: [0u8; 32],
        }.interp_key(), "natal:y");
        assert_eq!(DocOp::EditorPresence {
            interp_key: "natal:z".into(), joined: true,
        }.interp_key(), "natal:z");
    }

    #[test]
    fn edit_roundtrip() {
        let op = DocOp::Edit {
            interp_key:      "natal:sun_trine_moon".into(),
            base_rev:        sample_hash(),
            crdt_update:     vec![1, 2, 3, 4, 5],
            affected_blocks: vec![[0u8; 16], [1u8; 16]],
        };
        let bytes = op.encode();
        assert_eq!(op, DocOp::decode(&bytes).unwrap());
    }

    #[test]
    fn veto_roundtrip() {
        let op = DocOp::Veto {
            interp_key: "natal:x".into(),
            target_edit_op_id: sample_hash(),
        };
        assert_eq!(op, DocOp::decode(&op.encode()).unwrap());
    }

    #[test]
    fn affirm_rev_roundtrip() {
        let op = DocOp::AffirmRev {
            interp_key: "natal:venus_square_pluto".into(),
            target_rev: [42u8; 32],
        };
        assert_eq!(op, DocOp::decode(&op.encode()).unwrap());
    }

    #[test]
    fn presence_roundtrip() {
        let op = DocOp::EditorPresence {
            interp_key: "transit:mars_conjunction_sun".into(),
            joined:     true,
        };
        assert_eq!(op, DocOp::decode(&op.encode()).unwrap());
    }
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

// ── per-key log routing (Phase C-2) ─────────────────────────────────────────────

/// Derive a p2panda `log_id` from an `interp_key` so each key gets its own
/// per-author log — required for per-key sync topics to actually scope
/// what they replicate (see `docs/prd/granular-topic-subscription.md`).
pub fn log_id_for_key(interp_key: &str) -> u64 {
    let hash = blake3::hash(format!("interp-log:v1:{interp_key}").as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap())
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
        let op = InterpOp::Affirm { target_log_id: sample_hash() };
        let bytes = op.encode();
        assert_eq!(op, InterpOp::decode(&bytes).unwrap());
    }

    #[test]
    fn revoke_roundtrip() {
        let op = InterpOp::Revoke { target_log_id: sample_hash() };
        let bytes = op.encode();
        assert_eq!(op, InterpOp::decode(&bytes).unwrap());
    }

    #[test]
    fn respond_to_roundtrip() {
        let op = InterpOp::RespondTo {
            parent_log_id: sample_hash(),
            body:          "Yes, and also the body knows it.".into(),
        };
        let bytes = op.encode();
        assert_eq!(op, InterpOp::decode(&bytes).unwrap());
    }

    #[test]
    fn log_id_for_key_deterministic() {
        assert_eq!(log_id_for_key("natal:sun_trine_moon"), log_id_for_key("natal:sun_trine_moon"));
    }

    #[test]
    fn log_id_for_key_no_collisions_over_synthetic_keyspace() {
        // Every planet pair x every aspect kind x natal/transit/sky/house
        // prefix — a realistic upper bound on real key volume, larger than
        // any single author will plausibly publish to.
        const PLANETS: &[&str] = &[
            "sun", "moon", "mercury", "venus", "mars",
            "jupiter", "saturn", "uranus", "neptune", "pluto",
        ];
        const ASPECTS: &[&str] = &[
            "conjunction", "opposition", "square", "trine", "sextile",
            "quincunx", "semi_sextile", "semi_square", "sesquiquadrate",
        ];
        const PREFIXES: &[&str] = &["natal", "transit", "sky"];

        let mut seen = std::collections::HashSet::new();
        let mut count = 0usize;
        for prefix in PREFIXES {
            for a in PLANETS {
                for b in PLANETS {
                    if a == b { continue; }
                    for aspect in ASPECTS {
                        let key = format!("{prefix}:{a}_{aspect}_{b}");
                        count += 1;
                        assert!(seen.insert(log_id_for_key(&key)), "collision on {key}");
                    }
                }
            }
        }
        assert!(count > 2000, "sanity: expected a large synthetic keyspace, got {count}");
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
