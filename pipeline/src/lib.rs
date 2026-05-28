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
use std::collections::HashMap;

use p2panda_core::{Hash, Operation, VerifyingKey};
use p2panda_stream::{
    ComposedError, ComposedProcessors, LayeredBuilder, Pipeline, PipelineBuilder, Processor,
};
use thiserror::Error;
use tracing::{debug, trace};
use zodia_ops::{InterpOp, OpCodecError};

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
    /// Someone affirmed an interpretation.  The materialiser also tracks
    /// the running per-`interp_op_id` count internally and reports it.
    AffirmAdded {
        interp_op_id: Hash,
        voter:        VerifyingKey,
        running_count: u64,
    },
    /// Someone wrote a response that hangs off a parent interpretation.
    ResponseAdded {
        op_id:        Hash,
        author:       VerifyingKey,
        parent_op_id: Hash,
        body:         String,
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
                    match InterpOp::decode(&body.to_bytes()) {
                        Ok(interp) => DecodeOutput::Decoded { op: Box::new(op), interp },
                        Err(OpCodecError::Decode(msg)) => DecodeOutput::Skipped {
                            reason: SkipReason::MalformedOp(msg),
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
/// Keeps a small in-memory affirmation count map so each `Affirm` op can
/// emit a running total without round-tripping to the store.  The store
/// stays the source of truth; this is just enough to power UI optimism.
#[derive(Debug, Default)]
pub struct MaterializationProcessor {
    outbox:        RefCell<Vec<StateEvent>>,
    affirm_counts: RefCell<HashMap<Hash, u64>>,
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
                    InterpOp::Affirm { interp_op_id } => {
                        let mut counts = self.affirm_counts.borrow_mut();
                        let n = counts.entry(interp_op_id).or_insert(0);
                        *n += 1;
                        StateEvent::AffirmAdded {
                            interp_op_id,
                            voter: author,
                            running_count: *n,
                        }
                    }
                    InterpOp::RespondTo { parent_op_id, body } => StateEvent::ResponseAdded {
                        op_id, author, parent_op_id, body,
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
    async fn affirm_running_count_increments() {
        let sk_a = SigningKey::generate();
        let sk_b = SigningKey::generate();

        // Author op (whose hash A and B will affirm).
        let author_op = make_op(
            &sk_a,
            InterpOp::Author {
                interp_key: "natal:sun_square_pluto".into(),
                body:       "Transformation under pressure.".into(),
            }
            .encode(),
            0,
        );
        let target = author_op.header.hash();

        let affirm_a = make_op(
            &sk_a,
            InterpOp::Affirm { interp_op_id: target }.encode(),
            1,
        );
        let affirm_b = make_op(
            &sk_b,
            InterpOp::Affirm { interp_op_id: target }.encode(),
            0,
        );

        let pipe = ZodiaPipeline::new();
        pipe.process(author_op).await.unwrap();
        let _ = pipe.next().await.unwrap(); // InterpAuthored
        pipe.process(affirm_a).await.unwrap();
        let first = pipe.next().await.unwrap();
        pipe.process(affirm_b).await.unwrap();
        let second = pipe.next().await.unwrap();

        match (first, second) {
            (StateEvent::AffirmAdded { running_count: 1, .. },
             StateEvent::AffirmAdded { running_count: 2, .. }) => {}
            (a, b) => panic!("expected AffirmAdded counts 1 then 2, got {a:?} then {b:?}"),
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
