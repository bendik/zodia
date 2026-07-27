# PRD: Local storage pruning (Phase D, first slice)

**Status:** core mechanism shipped and tested — periodic automatic sweep not wired, affirm-exemption not implemented
**Branch:** `main`
**Foundation already landed:** `zodia-sync`'s per-key log-splitting (`docs/prd/granular-topic-subscription.md`), `zodia-sdk`'s command/reply facade (`docs/prd/zodia-sdk.md`).

## Problem Statement

`docs/prd/operations-and-streams-rearchitecture.md` bundled three different initiatives into "Phase D — Architectural reach": stream-negotiated pair channels (`zodia-channels`, replacing the ALPN-based consent handshake — a wire-format break), group encryption (`zodia-circles`, using p2panda 0.6 primitives that PRD's own Risks section flags as "newer/less battle-tested"), and a pruning processor. Of the three, pruning is the only one that doesn't require a new crate, a new wire format, or new cryptography — it was always meant to slot into the existing `zodia-pipeline` processor chain (`pipeline/src/lib.rs`'s own doc comment: "`PruningProcessor`... will land in Phase C/D once we have... a retention policy to drive them"). This PRD is that first, most tractable slice.

Without it: every operation a device ever receives — its own contributions and everyone else's — stays on local disk forever. Storage grows with total network activity, not with what a user actually reads. User story 20 from the original PRD: "As a Zodia user with limited storage, I want to configure my retention horizon... so that I trade off storage vs history depth on my own terms."

## Solution

`ZodiaClient::prune(retention: Duration) -> Result<u64, ClientError>`: deletes locally-stored operations older than `retention`, except anything authored by this device's own identity. Own contributions are *never* pruned regardless of age — losing your own authored history to a local housekeeping sweep would be a real data-loss bug, not a storage win.

Deliberately narrower than the original PRD's proposed default ("keep last 365 days + all of one's own contributions + any contribution one has affirmed, prune everything else" — Further Notes in the parent PRD): this first slice implements age + own-authorship only. The "exempt anything I've affirmed" rule is **not implemented** and not silently approximated — see Implementation Decisions for why it's a genuinely separate, harder problem, not a small addition.

## User Stories

1. As a Zodia user with a long-running install, I want old content I didn't author and haven't specially marked to eventually clear off my device, so storage doesn't grow unbounded with total community activity.

2. As a Zodia user, I want my own authored history to never be silently deleted by a local housekeeping sweep, regardless of how old it is.

3. As a Zodia developer, I want pruning to be safe to call at any time without needing to know which specific operations exist — a blanket "remove anything older than X, except mine" sweep, not a curated list.

## Implementation Decisions

### A real bug found before pruning could even be meaningful

Building this surfaced that operations **received** from a peer were never persisted locally at all — only self-published ones were. `zodia-sync`'s only `insert_operation` call site was inside `publish_bytes` (the self-publish path); the receive-path forwarder in `open_topic` took `TopicLogSyncEvent::OperationReceived { operation, .. }` and simply forwarded it to the pipeline for materialisation, never writing it to `operations_v1`. (Confirmed empirically, not just by code reading: a diagnostic query against a live cucumber scenario's store showed zero rows after a peer's edit was received and successfully materialised — the *content* was durable via `zodia-store`'s own tables, written by `app.rs`'s `StateEvent` handlers, but the *raw operation* was not.)

Practical consequence: a device could never re-serve received content to a third peer (no multi-hop relay — only an operation's original author could ever be its source), and pruning had nothing real to act on, since the only operations ever durably stored were self-authored ones (which are exempt from pruning by design). The pruning feature would have been correct code that was permanently a no-op.

Fixed with `store_and_associate` (`sync/src/lib.rs`): a shared free function doing insert + `TopicStore::associate` + commit in one transaction — the same insert-then-associate sequence `publish_bytes` already used for self-authored ops (see `docs/prd/granular-topic-subscription.md`'s "Bug found and fixed" note for the original half of this), generalised to *any* author. `associate` is the part that matters for relay: without it, an operation sits in `operations_v1` but is invisible to a third peer's `TopicStore::resolve` catch-up query. `open_topic`'s forwarder now calls this on every `OperationReceived`, using the operation's own `verifying_key` as the author (not `self`'s identity) and a `log_id` uniform across every author on that topic (log 0 for the legacy global topic, the key's derived log for a per-key topic — passed into `open_topic` at each of its three call sites, all of which already had it computed or trivially available).

### Why the affirm-exemption is deferred, not approximated

`DocOp::AffirmRev`'s target is a **doc revision hash** (`InterpDoc::current_rev()`, a CRDT-state-derived value), not an operation hash. A single revision can be the cumulative result of multiple `Edit` operations. Protecting "everything I've affirmed" correctly requires correlating a revision back to the specific operation(s) that produced it — information that lives in `zodia-doc`'s materialised state (`InterpDoc`, `doc_block_authors`), not in the raw operation log `zodia-sync` operates on. Implementing this properly would mean giving `zodia-sync` (or `zodia-sdk`) awareness of `zodia-store`'s materialised tables, a real architectural expansion, not a quick addition. Left as an explicit gap rather than hacked around with an approximation that might exempt the wrong thing (or nothing).

### What "own contributions" needed, that affirmed-content doesn't

By contrast, "never prune my own operations" only needs the `operations_v1.verifying_key` column — a direct equality check against the local identity's own public key, no CBOR decoding of operation bodies and no cross-referencing anything. This is why it shipped in this slice and the affirm-exemption didn't: one is a column comparison, the other needs information this layer doesn't have.

### Mechanism

`prune_older_than` (free function, `sync/src/lib.rs`) runs `DELETE FROM operations_v1 WHERE timestamp < ? AND verifying_key != ?` directly against the store's `SqlitePool` — bypassing `OperationStore::delete_operation`'s one-row-at-a-time interface for a single bulk statement (confirmed by reading `delete_operation`'s own implementation: it's exactly this same `DELETE ... WHERE hash = ?`, no other side effects, so a bulk equivalent is safe). Timestamps are stored as plain decimal text (matching `p2panda_store`'s own convention), so the comparison is a lexicographic string comparison — correct as long as both values have the same digit count, which microsecond-since-epoch timestamps do for the next ~260 years.

`ZodiaSyncNode::prune_older_than` wraps the free function with `self.signing_key.verifying_key()` as the exempt identity. `ZodiaClient::prune(retention)` (`zodia-sdk`) converts a `Duration` into an absolute `Timestamp` cutoff and routes it through the existing `Command`/reply-channel facade, alongside every other command.

`ZodiaSyncNode` itself needs a live network (`Endpoint`/`Gossip`) to construct, so — matching this crate's established testing pattern (`associate`'s tests) — `prune_older_than` and `store_and_associate` are both free functions taking `&SqliteStore` directly, testable with `SqliteStore::temporary()` and no network at all.

## Testing Decisions

- **Unit (`zodia-sync`, no network)**: `pruning_keeps_own_authored_ops_regardless_of_age` and `pruning_removes_old_ops_from_other_authors_but_keeps_recent_ones` — constructed operations with explicit past timestamps (`Timestamp::new(...)`, bypassing `Timestamp::now()`), proving the exemption and the age filter independently. `store_and_associate_makes_a_received_operation_discoverable_by_a_third_peer` regression-tests the persistence fix directly: an operation authored by someone other than "me" is stored and associated, then found via `TopicStore::resolve` — proving relay-discoverability, not just presence in the table.
- **End-to-end (`zodia-sdk` cucumber, real network, no mocking)**: `pruning.feature` — two scenarios. One publishes a real edit from Bob, has Alice receive and materialise it, waits a second, then prunes with zero retention and asserts at least one operation was removed — this is what caught the persistence bug in the first place (it failed against the pre-fix code, correctly). The other has Alice author her own content and prune with zero retention, asserting exactly zero operations were removed.

No synthetic backdated operations were needed for the cucumber layer — real publish, real receipt, real elapsed time (however briefly) was enough to prove the real flow, which is stronger evidence than a mocked backdate would have been.

## Out of Scope

- **Automatic periodic sweep.** `prune()` is on-demand only — nothing calls it on a timer yet. Matches this session's established pattern of shipping a callable primitive first (`touch_subscription` shipped the same way, then got wired into `app.rs`'s page lifecycle in a follow-up commit). Wiring a periodic call (daily is plenty, given the intended retention horizons are measured in months) into either `zodia-sdk`'s background loop or `app.rs` is the natural next step.
- **The affirm-exemption.** See Implementation Decisions above — a real, separate design question needing `zodia-store`/`zodia-doc` awareness, not attempted here.
- **A user-configurable retention horizon.** `retention` is a caller-supplied `Duration` with no UI to set it yet, and no persisted user preference. The original PRD's "keep last 90 days" vs "365 days" framing was illustrative, not a decided default — deferred alongside the exemption question since both want the same follow-up design pass.
- **The other two Phase D initiatives** (`zodia-channels` stream-negotiated pair channels, `zodia-circles` group encryption). Both remain untouched, unstarted, and — per the parent PRD's own Risks section — meaningfully riskier than this slice.

## Further Notes

**Why this session stopped to report the persistence bug rather than just fixing it.** The gap (received operations never durably stored) is significantly bigger in scope than "the pruning test doesn't pass" — it affects the core P2P relay model (a device could only ever source its own contributions to peers, never relay what it had received from someone else) independent of whether pruning exists at all. That's the kind of finding worth surfacing and getting explicit direction on before folding into a feature branch, not something to quietly patch as a means to an end. Confirmed with the user before implementing: fix persistence first, then finish pruning — recorded here as the reasoning trail, not just the outcome.
