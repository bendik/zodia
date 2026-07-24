//! Inbound-operation pipeline.
//!
//! Sits between the raw `p2panda-net::sync::LogSync` event stream and the
//! relm4 app layer.  Operations arrive at one end as `Operation<()>`; they
//! flow through a stack of [`p2panda_stream::Processor`]s and emerge at
//! the other end as typed [`StateEvent`]s the app consumes.
//!
//! # Phase A scope
//!
//! Only [`DecodeProcessor`] and [`MaterializationProcessor`] do real work
//! today.  The three placeholder processors that the PRD calls for —
//! `CausalOrderingProcessor`, `AccessControlProcessor`, `PruningProcessor`
//! — will land in Phase C / D once we have multi-author threads, circle
//! encryption, and a retention policy to drive them.  The plumbing here
//! is already shaped so adding them is `pipeline_builder.layer(...)` plus
//! the processor's own logic.
//!
//! # Threading model
//!
//! `p2panda-stream` is `!Send`; callers must drive [`ZodiaPipeline`] from
//! a tokio current-thread runtime / `LocalSet`.  In Zodia this is the
//! relm4 main loop's runtime.

use std::cell::RefCell;

use p2panda_core::{Hash, Operation, VerifyingKey};
use p2panda_stream::{
    ComposedError, ComposedProcessors, LayeredBuilder, Pipeline, PipelineBuilder, Processor,
};
use thiserror::Error;
use tracing::{debug, trace};
use zodia_ops::{DocOp, InterpOp, OpCodecError};

// ── public types ──────────────────────────────────────────────────────────────

/// Pipeline output — derived state events the app consumes to update its UI.
///
/// Each variant represents the *materialised* consequence of an `InterpOp`
/// being processed: not the op itself but what it means downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateEvent {
    /// A new authored interpretation reached us.
    InterpAuthored {
        op_id:      Hash,
        author:     VerifyingKey,
        interp_key: String,
        body:       String,
    },
    /// Someone affirmed an interpretation, identified by its content hash
    /// (`BLAKE3(interp_key || body)`).  Downstream, the app inserts a
    /// `(target_log_id, voter)` row into the local affirmations table —
    /// the store is the source of truth for the count, since the pipeline's
    /// view only reflects events seen since startup.
    AffirmAdded {
        target_log_id: Hash,
        voter:         VerifyingKey,
    },
    /// Someone wrote a response that hangs off a parent interpretation.
    /// `parent_log_id` is the content hash of the parent — orphan responses
    /// (parent not yet known locally) still get persisted; the join-on-display
    /// resolves them when the parent eventually arrives.
    ResponseAdded {
        op_id:         Hash,
        author:        VerifyingKey,
        parent_log_id: Hash,
        body:          String,
    },
    /// An authored interpretation was revoked by its original author.
    /// `by` is the revoker's verifying key — downstream materialisation
    /// must confirm `by == author_of(target_log_id)` before applying the
    /// tombstone (drops impostor revokes).
    InterpRevoked {
        op_id:         Hash,
        by:            VerifyingKey,
        target_log_id: Hash,
    },
    /// Phase F-collab: a CRDT edit landed against `interp_key`.  The
    /// materialiser persists the update into the local Loro doc and updates
    /// per-block author rings.
    DocEdited {
        op_id:           Hash,
        by:              VerifyingKey,
        interp_key:      String,
        base_rev:        Hash,
        crdt_update:     Vec<u8>,
        affected_blocks: Vec<[u8; 16]>,
        timestamp:       u64,
    },
    /// Phase F-collab: a peer proposed a veto.  Downstream authority check
    /// (ring + window + newest-edit) lives in the app handler since it
    /// needs store access.
    DocVetoProposed {
        op_id:             Hash,
        by:                VerifyingKey,
        interp_key:        String,
        target_edit_op_id: Hash,
        timestamp:         u64,
    },
    /// Phase F-collab: an affirmation against (interp_key, revision).
    DocAffirmed {
        op_id:      Hash,
        by:         VerifyingKey,
        interp_key: String,
        target_rev: [u8; 32],
    },
    /// Phase F-collab: presence heartbeat for the per-key editor session.
    /// `joined = false` means "I left."
    EditorPresenceChanged {
        op_id:      Hash,
        by:         VerifyingKey,
        interp_key: String,
        joined:     bool,
        timestamp:  u64,
    },
    /// An op was decoded but skipped — malformed body, unsupported variant,
    /// missing parent, denied access, etc.  Reported for observability so
    /// the UI / logs can surface "0 dropped, 0 deferred" counts later.
    Skipped {
        reason: SkipReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `Operation.body` was `None`.
    NoBody,
    /// CBOR didn't shape-match any `InterpOp` variant.
    MalformedOp(String),
}

// ── errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("pipeline closed")]
    Closed,
}

// ── decode processor ──────────────────────────────────────────────────────────

/// Pipeline stage: `Operation<()>` → either a decoded `(Operation, InterpOp)`
/// pair or a `StateEvent::Skipped` if the body is missing/malformed.
///
/// Pass-through behaviour for downstream layers: `Decoded` flows on,
/// `Skipped` short-circuits straight to the app as a `StateEvent`.
#[derive(Debug, Clone, Default)]
pub struct DecodeProcessor {
    inbox:  RefCell<Vec<Operation<()>>>,
    outbox: RefCell<Vec<DecodeOutput>>,
}

#[derive(Debug, Clone)]
pub enum DecodeOutput {
    /// `op` is boxed because `Operation<()>` is ~400 bytes and the `Skipped`
    /// variant is tiny — keeps the common case from inflating the enum.
    Decoded { op: Box<Operation<()>>, interp: InterpOp },
    /// Phase F-collab: op body decoded as a `DocOp` instead.
    DecodedDoc { op: Box<Operation<()>>, doc: DocOp },
    Skipped { reason: SkipReason },
}

impl Processor<Operation<()>> for DecodeProcessor {
    type Output = DecodeOutput;
    type Error  = PipelineError;

    async fn process(&self, input: Operation<()>) -> Result<(), Self::Error> {
        self.inbox.borrow_mut().push(input);
        // Decode synchronously into outbox; in a more elaborate stage this
        // could batch, dedupe, or apply back-pressure.
        let mut pending = self.inbox.borrow_mut();
        while let Some(op) = pending.pop() {
            let out = match &op.body {
                None => DecodeOutput::Skipped { reason: SkipReason::NoBody },
                Some(body) => {
                    let bytes = body.to_bytes();
                    // Try InterpOp first (legacy wire format), then DocOp.
                    match InterpOp::decode(&bytes) {
                        Ok(interp) => DecodeOutput::Decoded { op: Box::new(op), interp },
                        Err(_) => match DocOp::decode(&bytes) {
                            Ok(doc)  => DecodeOutput::DecodedDoc { op: Box::new(op), doc },
                            Err(OpCodecError::Decode(msg)) => DecodeOutput::Skipped {
                                reason: SkipReason::MalformedOp(msg),
                            },
                        },
                    }
                }
            };
            self.outbox.borrow_mut().push(out);
        }
        Ok(())
    }

    async fn next(&self) -> Result<Self::Output, Self::Error> {
        loop {
            if let Some(out) = self.outbox.borrow_mut().pop() {
                trace!(?out, "decode → next");
                return Ok(out);
            }
            // Yield once so the runtime can deliver more inputs via process().
            // No internal scheduling — caller drives via process+next pairs.
            tokio::task::yield_now().await;
        }
    }
}

// ── materialization processor ────────────────────────────────────────────────

/// Pipeline stage: `DecodeOutput` → `StateEvent`.
///
/// Stateless today: each op maps to exactly one `StateEvent`.  The
/// downstream app handler owns side-effects (store writes, UI refresh).
#[derive(Debug, Default)]
pub struct MaterializationProcessor {
    outbox: RefCell<Vec<StateEvent>>,
}

impl Processor<DecodeOutput> for MaterializationProcessor {
    type Output = StateEvent;
    type Error  = PipelineError;

    async fn process(&self, input: DecodeOutput) -> Result<(), Self::Error> {
        let event = match input {
            DecodeOutput::Skipped { reason } => StateEvent::Skipped { reason },
            DecodeOutput::Decoded { op, interp } => {
                let op_id  = op.header.hash();
                let author = op.header.verifying_key;
                match interp {
                    InterpOp::Author { interp_key, body } => StateEvent::InterpAuthored {
                        op_id, author, interp_key, body,
                    },
                    InterpOp::Affirm { target_log_id } => StateEvent::AffirmAdded {
                        target_log_id,
                        voter: author,
                    },
                    InterpOp::RespondTo { parent_log_id, body } => StateEvent::ResponseAdded {
                        op_id, author, parent_log_id, body,
                    },
                    InterpOp::Revoke { target_log_id } => StateEvent::InterpRevoked {
                        op_id, by: author, target_log_id,
                    },
                }
            }
            DecodeOutput::DecodedDoc { op, doc } => {
                let op_id  = op.header.hash();
                let by     = op.header.verifying_key;
                let ts: u64 = u64::from(op.header.timestamp);
                match doc {
                    DocOp::Edit { interp_key, base_rev, crdt_update, affected_blocks } =>
                        StateEvent::DocEdited {
                            op_id, by, interp_key, base_rev, crdt_update,
                            affected_blocks, timestamp: ts,
                        },
                    DocOp::Veto { interp_key, target_edit_op_id } => StateEvent::DocVetoProposed {
                        op_id, by, interp_key, target_edit_op_id, timestamp: ts,
                    },
                    DocOp::AffirmRev { interp_key, target_rev } => StateEvent::DocAffirmed {
                        op_id, by, interp_key, target_rev,
                    },
                    DocOp::EditorPresence { interp_key, joined } =>
                        StateEvent::EditorPresenceChanged {
                            op_id, by, interp_key, joined, timestamp: ts,
                        },
                }
            }
        };
        debug!(?event, "materialised");
        self.outbox.borrow_mut().push(event);
        Ok(())
    }

    async fn next(&self) -> Result<Self::Output, Self::Error> {
        loop {
            if let Some(event) = self.outbox.borrow_mut().pop() {
                return Ok(event);
            }
            tokio::task::yield_now().await;
        }
    }
}

// ── high-level pipeline ──────────────────────────────────────────────────────

/// Front-door type Zodia callers use.  Hides the concrete `Pipeline<...>`
/// composed type so adding new layers later doesn't ripple into callers.
pub struct ZodiaPipeline {
    inner: Pipeline<ComposedProcessors<DecodeProcessor, MaterializationProcessor>>,
}

impl ZodiaPipeline {
    /// Build the default Zodia pipeline.
    ///
    /// Today: `Decode → Materialize`.  Future phases insert
    /// `CausalOrdering`, `AccessControl`, `Pruning` between the two.
    pub fn new() -> Self {
        let layered: LayeredBuilder<DecodeProcessor, Operation<()>> =
            PipelineBuilder::<Operation<()>>::new().layer(DecodeProcessor::default());
        let layered = layered.layer(MaterializationProcessor::default());
        Self { inner: layered.build() }
    }

    /// Feed one raw operation into the pipeline.  Back-pressure-aware:
    /// returns once the operation is queued (not necessarily processed).
    pub async fn process(&self, op: Operation<()>) -> Result<(), PipelineError> {
        self.inner.process(op).await.map_err(flatten_composed_err)
    }

    /// Await the next state event.  Never returns `Ok(None)` — pipelines
    /// stay open until dropped.
    pub async fn next(&self) -> Result<StateEvent, PipelineError> {
        self.inner.next().await.map_err(flatten_composed_err)
    }
}

/// Both wrapped processors share the same `PipelineError` type, so flatten
/// `ComposedError<PipelineError, PipelineError>` back to a single
/// `PipelineError` for the public API.
fn flatten_composed_err(e: ComposedError<PipelineError, PipelineError>) -> PipelineError {
    match e {
        ComposedError::First(e)  => e,
        ComposedError::Second(e) => e,
    }
}

impl Default for ZodiaPipeline {
    fn default() -> Self { Self::new() }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use p2panda_core::{Body, Header, SigningKey, Timestamp};

    fn make_op(signing_key: &SigningKey, body_bytes: Vec<u8>, seq_num: u64) -> Operation<()> {
        let body = Body::new(&body_bytes);
        let mut header = Header::<()> {
            version:       1,
            verifying_key: signing_key.verifying_key(),
            signature:     None,
            payload_size:  body.size(),
            payload_hash:  Some(body.hash()),
            timestamp:     Timestamp::now(),
            seq_num,
            backlink:      None,
            extensions:    (),
        };
        header.sign(signing_key);
        let hash = header.hash();
        Operation { hash, header, body: Some(body) }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn author_op_yields_interp_authored() {
        let sk = SigningKey::generate();
        let op = make_op(
            &sk,
            InterpOp::Author {
                interp_key: "natal:venus_trine_jupiter".into(),
                body:       "Easy generosity, lucky in love.".into(),
            }
            .encode(),
            0,
        );
        let pipe = ZodiaPipeline::new();
        pipe.process(op.clone()).await.unwrap();
        let ev = pipe.next().await.unwrap();
        match ev {
            StateEvent::InterpAuthored { author, interp_key, body, op_id } => {
                assert_eq!(author, sk.verifying_key());
                assert_eq!(interp_key, "natal:venus_trine_jupiter");
                assert_eq!(body, "Easy generosity, lucky in love.");
                assert_eq!(op_id, op.header.hash());
            }
            other => panic!("expected InterpAuthored, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn two_voters_yield_two_distinct_affirm_events() {
        let sk_a = SigningKey::generate();
        let sk_b = SigningKey::generate();

        // Any 32-byte hash will do as the content target; the pipeline
        // doesn't dereference it, downstream does.
        let target = {
            let helper = make_op(&sk_a, b"target-fixture".to_vec(), 0);
            helper.header.hash()
        };

        let affirm_a = make_op(
            &sk_a,
            InterpOp::Affirm { target_log_id: target }.encode(),
            1,
        );
        let affirm_b = make_op(
            &sk_b,
            InterpOp::Affirm { target_log_id: target }.encode(),
            0,
        );

        let pipe = ZodiaPipeline::new();
        pipe.process(affirm_a).await.unwrap();
        let first = pipe.next().await.unwrap();
        pipe.process(affirm_b).await.unwrap();
        let second = pipe.next().await.unwrap();

        match (first, second) {
            (
                StateEvent::AffirmAdded { target_log_id: t1, voter: v1 },
                StateEvent::AffirmAdded { target_log_id: t2, voter: v2 },
            ) => {
                assert_eq!(t1, target);
                assert_eq!(t2, target);
                assert_eq!(v1, sk_a.verifying_key());
                assert_eq!(v2, sk_b.verifying_key());
                assert_ne!(v1, v2, "voters should be distinct");
            }
            (a, b) => panic!("expected two AffirmAdded events, got {a:?} then {b:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_body_yields_skipped() {
        let sk = SigningKey::generate();
        let op = make_op(&sk, vec![0xff, 0xfe, 0xfd], 0);
        let pipe = ZodiaPipeline::new();
        pipe.process(op).await.unwrap();
        match pipe.next().await.unwrap() {
            StateEvent::Skipped { reason: SkipReason::MalformedOp(_) } => {}
            other => panic!("expected Skipped MalformedOp, got {other:?}"),
        }
    }
}
