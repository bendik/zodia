# PRD: Operations-and-streams rearchitecture

**Status:** partially shipped — Phases A, B, C-1, C-2 shipped; D partially shipped (pruning slice + p2panda 0.7 migration prerequisite), `zodia-channels`/`zodia-circles` not started
**Branch:** `main`
**Foundation already landed:** 0.6.0 (iroh 0.98 + p2panda 0.6 + sqlx-backed store), upgraded to p2panda 0.7.0 / iroh 1.0.3 per `docs/prd/p2panda-0.7-migration.md`

## Problem Statement

Zodia's networking and sync layer is currently structured around two transports that don't share a common substrate:

- **Tier-0 + Tier-1 over raw iroh QUIC + a custom ALPN (`zodia/tier1/1`)** for discovery, consent exchange, presence, chat, and AV signaling. Custom ALPN code paths, custom framing, custom retry logic.
- **`LogSync` over p2panda gossip** for community interpretations only.

This split has visible costs for users:

- **The community signal is local.** Affirmations (the ♡ count) are only counted on the device that issued them. Two users browsing the same community interpretation see different ranks. The "community body" is therefore disconnected.
- **Contributions are flat.** A peer can't riff on another peer's interpretation — every contribution stands alone with no causal link.
- **Topic subscription is rigid and eager.** Every node subscribes to its chart's 22 aspect topics at startup, regardless of whether the user ever browses those keys. Keys outside the user's own chart are unreachable through sync.
- **Adding new pair capabilities is heavyweight.** Any new feature (e.g. presence-rich status, calendar sharing, video) needs new ChannelMsg variants, new code paths in the consent handler, new persistence layers.
- **No visibility into the network state.** Users see "I'm online" but not "I've caught up with 5 of 12 known peers" or "syncing 234/237 with X" — the p2p layer is invisible.
- **No way to share interpretations with a smaller circle than "the entire network".** It's all-or-nothing public.
- **Storage grows unboundedly.** Old operations never get pruned.

## Solution

Restructure the networking + sync layer around two foundational ideas:

1. **Everything valuable is a p2panda operation** flowing through one shared transport. Authored interpretations, affirmations, responses, presence — same wire format, same replication mechanism.
2. **The topic IS the rendezvous, the stream IS the capability.** Pair channels become a private, encrypted topic with typed sub-streams for chat, AV signaling, etc. — no special-case ALPN, no custom framing.

A `p2panda-stream::Pipeline` becomes the inbound spine: raw gossip / sync events enter at one end, are decoded → causally ordered → access-controlled → materialised into derived state events at the other, and the relm4 layer consumes a single typed event stream. The manual `network_changed_token += 1` pattern disappears — derived state is a function of the op log.

For users this materialises as: community ranking that converges across all peers, conversational threads of interpretation, sync progress in the UI, private circles, and bounded local storage — all built on the same foundation rather than four separate features.

## User Stories

1. As a Zodia user, I want the ♡ counts on community interpretations to be the same on my device as on my friend's device, so that the community ranking actually represents the community.

2. As a Zodia user, I want to affirm an interpretation and have that ♡ visible to other peers within a few seconds (when online) or once they catch up (when offline), so that my taste contributes to the shared ranking.

3. As a Zodia contributor, I want to respond to another peer's interpretation with a riff of my own, so that contributions can build into conversations instead of standing alone.

4. As a Zodia reader, I want to see a contribution and the responses it spawned grouped together in a thread, so that I can follow the conversation rather than reading flat, disconnected lists.

5. As a Zodia reader browsing an aspect that isn't in my own chart, I want to fetch and read community interpretations for that aspect, so that I can explore the community body beyond my personal chart.

6. As a Zodia user on a metered connection, I don't want my device to subscribe to topics I'm not actively browsing, so that background sync doesn't waste bandwidth.

7. As a Zodia user opening the app, I want to see which of my known peers I'm caught up with, so that I have realistic expectations about how fresh the community ranking is.

8. As a Zodia user reconnecting after being offline, I want to see a sync progress indicator while my device catches up, so that I know the empty feed will fill in once sync completes.

9. As a Zodia user who shared an interpretation in a private friend circle, I want only members of that circle to be able to read it, so that I can share intimate readings without publishing to everyone.

10. As a Zodia user, I want to revoke a friend's access to my private circle and rotate the circle's key, so that I retain control of who reads my private contributions over time.

11. As a Zodia user, I want my device's storage to stay bounded over years of use, so that the app doesn't grow to gigabytes as the community produces more content.

12. As a Zodia user pairing with a new peer, I don't want to wait for a separate "consent ALPN handshake" — the act of agreeing to connect should immediately enable chat and presence with no extra round trips, so that pairing feels instant.

13. As a Zodia user with a paired peer who comes online, I want chat, sync, and AV-call availability to be negotiated and ready without my interaction, so that the connection feels live rather than something I have to manage.

14. As a Zodia developer adding a new pair capability (e.g. shared calendar), I want to add a typed sub-stream to the existing pair-channel rather than design a new ALPN protocol, so that capability addition is one-day work rather than one-week work.

15. As a Zodia developer debugging a sync issue, I want sync events to flow through a single inspectable pipeline rather than being scattered across `network_changed_token` bumps, so that I can trace what happened to a single op end-to-end.

16. As a Zodia user, I want my affirmations and contributions to survive an app restart in transit (not just after they reach a peer), so that a crash mid-publish doesn't lose the op.

17. As a Zodia user resyncing after a long offline gap, I don't want every reconnect to redownload my entire history — only the gap, so that catch-up is fast.

18. As a Zodia user, I want the affirmation count on an interpretation to be sybil-resistant (one ♡ per pubkey per interp), so that the ranking signal stays meaningful as the network grows.

19. As a Zodia user reading a threaded conversation, I want to see the original interpretation and its responses in causal order even if they arrived out of order from the network, so that the thread reads naturally regardless of network timing.

20. As a Zodia user with limited storage, I want to configure my retention horizon (e.g. "keep last 90 days, keep starred interpretations forever") so that I trade off storage vs history depth on my own terms.

## Implementation Decisions

### Operation model

A single canonical op enum lives in a new `zodia-ops` crate:

- `InterpOp::Author { interp_key, body }` — the existing authored-interpretation case, repackaged.
- `InterpOp::Affirm { interp_op_id }` — affirmations become first-class ops, replicated and counted globally.
- `InterpOp::RespondTo { parent_op_id, body }` — explicit causal parent for threaded contributions.

Wire format is CBOR. Each op is the body of a p2panda `Operation<()>`; the p2panda `Header` provides per-op signature, sequence number, and timestamp. The Zodia-level signature (`author_sig` in current `InterpEntry`) is dropped — the p2panda header signature is the canonical authentication. `zodia-store::insert_received` is updated accordingly.

Sybil resistance for affirmations is enforced at the materialisation layer: per `interp_op_id`, count distinct `verifying_key`s that emitted an `Affirm` op for it.

### Pipeline as inbound spine

A new `zodia-pipeline` crate wraps `p2panda-stream::Pipeline` and exposes one input (incoming raw operations from gossip + sync) and one output (typed `StateEvent`s the relm4 layer consumes). The processors stacked inside, in order:

1. **DecodeProcessor** — bytes → `InterpOp` (drop malformed).
2. **CausalOrderingProcessor** — buffer ops referencing parents we haven't yet seen; release in causal order. Built on p2panda's `Cursor` and `LogHeights` primitives.
3. **AccessControlProcessor** — for ops in a circle topic, decrypt with circle key; drop if not a member.
4. **MaterializationProcessor** — translate ops into `StateEvent`s: `InterpAdded`, `AffirmAdded { count }`, `ThreadUpdated { root, descendants }`, etc.
5. **PruningProcessor** — applies retention policy; emits `OpEvicted { hash }` and removes from store.

Pipeline output replaces the current `network_changed_token` bumps. The relm4 layer subscribes to `StateEvent`s and updates its derived state explicitly per event type rather than refreshing everything on a generic dirty flag.

### Topic granularity and lazy subscription

Topic management moves into `zodia-core` (small enough to not deserve its own crate). Three classes of topic:

- **Per-interp-key topics**: `Topic::from(blake3("interp:" || interp_key))`. The app subscribes lazily — on entering an aspect detail page, subscribe; on leaving for >N minutes, unsubscribe. Always subscribed: the topics in the user's own chart (since those drive the home view).
- **Pair-channel topics**: `Topic::from(blake3("pair:" || sorted(pubkey_a, pubkey_b)))`. Private, encrypted, one per pair. The connection IS the subscription.
- **Circle topics**: `Topic::from(blake3("circle:" || circle_id))`. Encrypted to a group key.

The global "everyone announces here" gossip topic is retained for peer discovery only (Tier-0 announce blobs continue to flow there).

### Stream-negotiated pair channels

A new `zodia-channels` crate replaces the current `CONSENT_PROTOCOL` ALPN. `PairChannel::open(peer)` joins the pair-channel topic; the first messages exchanged are typed capability claims (e.g. `Capability::Chat`, `Capability::AvCall`, `Capability::InterpSync`). Each agreed capability becomes a typed sub-stream the consumer can `.next().await` on. The consent blob (birth data, prekeys) is the first op exchanged on the pair topic; once both sides accept, the channel is live.

This replaces:

- The custom ALPN registration in `zodia-net::network::ConsentHandler`.
- The current `ChannelMsg` enum and `DirectChannel` framing.
- The `IncomingChannel` / `ConnectionComplete` round-trips in `app::AppModel`.

The AV layer (`zodia-av`) still uses raw iroh QUIC streams for media bandwidth reasons. The trigger to open those streams moves to the capability-negotiation in `zodia-channels` — when both sides agree on `Capability::AvCall`, `zodia-av` opens the actual media streams against the same shared iroh `Endpoint`.

### Group encryption

A new `zodia-circles` crate provides circle creation, membership management, key rotation, encrypt/decrypt of `InterpOp` bodies. Wraps p2panda 0.6's group-encryption primitives. Circle membership is itself a small set of ops (CircleCreate, MemberAdd, MemberRevoke, KeyRotate) on a hidden bootstrap topic between members.

A user-facing "Share with..." picker on each contribution decides whether it goes to the public network (current behaviour) or to a named circle. Reading a circle interp requires being a current member; revocation + rotation means a removed member can read history they already cached but not future ops.

### Why not `p2panda::Node`

`p2panda::Node` is the convenience wrapper around `p2panda-net + p2panda-sync + p2panda-store + p2panda-stream`. We use every one of those primitives directly. We don't use `Node` itself because it owns its iroh `Endpoint` internally and doesn't expose it, and the AV layer needs raw QUIC access to that same endpoint. Going through `Node` would force two iroh endpoints (Node's + ours for AV). Composing one level lower keeps all the same features on one endpoint.

### Existing-crate modifications

- **`zodia-store`**: schema additions for derived state — `affirmations(interp_op_id, voter_pk)` (replaces the legacy local-only table; counts are now a query), `interp_parent(interp_op_id, parent_op_id)`, `circle_membership(circle_id, member_pk, role, joined_at)`. The pipeline's MaterializationProcessor is the only writer for these.
- **`zodia-sync`**: shrinks. Its job becomes "feed the LogSync receive stream into the Pipeline input." Public `ZodiaSyncNode::publish` is retained but now accepts a typed `InterpOp` rather than `(key, body, sig)`.
- **`zodia-net`**: drop the `ConsentHandler` ProtocolHandler and `CONSENT_PROTOCOL` ALPN. Drop `DirectChannel` framing. `ZodiaNetwork` keeps endpoint + address book + gossip + discovery + the Tier-0 announce loop.
- **`zodia-av`**: unchanged except the trigger — driven by `zodia-channels::PairChannel` capability events instead of `ChannelMsg::CallOffer`.
- **`app/`**: `AppMsg` shrinks substantially; many variants (`AcceptConsent`, `RejectConsent`, `IncomingChannel`, `ProposeConsent`, manual `Reconnect`, `ShareInterp`, `AffirmInterp`, `SubmitInterp`, `SyncInterpReceived`, `InterpReceived`, etc.) collapse into a small set sourced from Pipeline `StateEvent`s.

### Persistence + restart durability

`p2panda-store::SqliteStore` already persists the operation log across restarts (the 0.6 migration landed this). The new ops (Affirm, RespondTo) inherit that durability. Mid-publish crash recovery: an op inserted into the local store but not yet broadcast survives the restart; on next online connection LogSync replays from the local tip.

### Phasing (from the planning conversation)

- **Phase A — Foundation** (shipped 0.7.0): `zodia-ops` + `zodia-pipeline` scaffolding. Wire inbound LogSync stream through the Pipeline; route Pipeline `StateEvent`s into the existing `AppMsg` flow as a one-for-one substitute. No new behaviour yet, but the spine is in place.
- **Phase B — First behavioural payoffs** (shipped 0.7.0): network-replicated affirmations; sync metrics in UI.
- **Phase C-1 — Causal threads** (shipped 0.7.0): `RespondTo` + ordering. See `docs/prd/activity-feed.md` and `docs/prd/collaborative-interpretations.md` for the UI-layer phases (E, F-collab) that cashed this in — both shipped together as 0.7.1.
- **Phase C-2 — Granular subscription** (not started): per-key topics with lazy subscribe/unsubscribe. Deferred by both Phase E and Phase F-collab PRDs. Drafted in `docs/prd/granular-topic-subscription.md`, which also corrects this section's original topic-partitioning sketch against what Phase A actually shipped (single global topic, single per-author log).
- **Phase D — Architectural reach** (pruning slice shipped; p2panda 0.7 migration shipped as a prerequisite; `zodia-channels`/`zodia-circles` not started): stream-negotiated pair channels (`zodia-channels`, dropping ALPN) and group encryption (`zodia-circles`) remain untouched — both bigger and riskier than pruning per this PRD's own Risks section. The pruning processor's first slice (age + own-authorship exemption, on-demand) is drafted and shipped in `docs/prd/pruning.md`, which also documents a real bug found while building it: received (not self-authored) operations were never durably persisted at all, meaning this device could never relay them to a third peer. Fixed as part of that PRD. Referenced as a prerequisite for the later real-time-presence and audio phases (H/I) in `collaborative-interpretations.md`. Before `zodia-circles` could start for real, the whole p2panda stack needed 0.6→0.7 (this PRD's own text about "wrapping p2panda 0.6's group-encryption primitives" was wrong — that crate doesn't exist at 0.6.x) — see `docs/prd/p2panda-0.7-migration.md` for what that upgrade actually touched, including a schema-change regression in the just-shipped pruning feature that its own cucumber tests caught. `zodia-circles` itself is now drafted in `docs/prd/circles.md`, grounded in the real 0.7.0 `p2panda-spaces`/`p2panda-auth`/`p2panda-encryption` APIs — it turns out to be a thin wrapper over `p2panda-spaces::Space` rather than the from-scratch group-crypto crate originally sketched here.

Each phase is independently shippable; A is a no-user-visible foundation, B-D each ship a 0.x.0 bump with observable user value.

## Testing Decisions

A good test for this work asserts external behaviour — what an op consumer or capability consumer observes — not implementation details of which processor stage fired in which order. We test the public API of each new crate against a fake input source, not the internal wiring.

The four new crates each get a focused test surface:

- **`zodia-ops` codec**: round-trip ser/de tests for every `InterpOp` variant. Backwards-compatibility cases once a wire format is in place (extra fields in CBOR must decode cleanly into older variants).
- **`zodia-pipeline` processors**: each processor tested in isolation by feeding a fake `Stream<InterpOp>` in and asserting `StateEvent`s out. The CausalOrderingProcessor specifically needs out-of-order arrival tests (parent arriving after child) and missing-parent tests (child held until parent arrives, GC after timeout). MaterializationProcessor tests assert state events for representative op sequences match the expected derived shape.
- **`zodia-channels` handshake**: capability negotiation outcomes — both sides agree, partial agreement (one side wants chat, other doesn't), full disagreement, network partition mid-handshake. Uses a mock topic-and-stream backend so iroh isn't required for the tests.
- **`zodia-circles` encryption**: round-trip encrypt/decrypt for circle membership; key rotation correctness (after rotation, old key can't decrypt new ops; new members get the new key); revocation behaviour (removed member can decrypt history but not future).

We don't write integration tests against live iroh networking in this work — those tests exist at the `p2panda-net` / `iroh` level upstream, and our value-add tests live above that layer.

Prior art for these tests: there are currently no unit tests of meaningful coverage in the workspace (Phase A is the first time we'll need to be disciplined about this). The stylistic model is the existing tests under `net/tests/channel.rs` for crate-level integration shape, but the new tests will lean async-heavier and use mock backends rather than real iroh.

## Out of Scope

- **AV protocol rework**: real-time audio media stays on raw iroh QUIC streams over the shared iroh `Endpoint`. Only the *trigger* to open those streams moves to capability negotiation. Replacing the underlying RTP-ish framing or migrating to a different media protocol is a separate concern.
- **Replacing iroh itself**: this PRD assumes p2panda's iroh-based transport. Switching to a non-iroh QUIC stack would change every layer below `zodia-channels`.
- **Federation / bridging to other p2p networks**: not contemplated here. The "network" remains the global Zodia network defined by `NETWORK_ID`.
- **Identity recovery / multi-device**: each device still uses its own identity keypair derived from a local seed. Sharing one identity across devices is a separate feature with its own threat model.
- **Web / mobile clients**: Linux desktop only for this PRD. The new ops and stream contracts are wire-format-stable so future clients can interop, but their UI and platform integration are not in scope.
- **Compaction / re-encoding of existing data**: existing 0.6.x users' local stores keep working. Migration from `interpretations.db`'s legacy `affirmations` table to the new ops-derived counts happens lazily on first read of each key (or via a one-time migration on first 0.7+ launch — exact strategy decided during Phase B).
- **Spam / abuse mitigation beyond sybil-per-pubkey**: out of scope. Rate limiting, content moderation, reputation, etc. are their own design discussion.
- **Topic discovery beyond the global announce + per-interp-key topics**: ambient discovery of "trending circles" or "popular keys near you" is out of scope.

## Further Notes

**Why a PRD before code.** This refactor crosses every crate in the workspace and touches the wire format. Capturing the contract — ops, pipeline events, capability streams — before writing it down in code is the difference between a 6-week migration and a 12-week one with rework.

**Migration story.** Each phase is shippable. Phase A is invisible. Phase B adds new ops that are forwards-compatible with peers still on 0.6.x (they receive the new op types but their pipeline doesn't yet materialise affirmation/response state — their counts stay local until they upgrade). Phase C extends the wire format with RespondTo's parent reference. Phase D replaces the pair channel transport — at the version cutover, two peers on different sides of the cut won't be able to pair until both upgrade. Plan: Phase D ships as 1.0.0 with a known incompatibility note in the release.

**Open questions to resolve during Phase A.**

- Exact CBOR field naming for `InterpOp` (small bikeshed, decide in PR).
- Whether the pipeline runs on the same tokio runtime as relm4 (probably yes; p2panda-stream is `!Send` so we need a local runtime — relm4's main loop integration is the natural home).
- Whether circles need a directory / discovery mechanism, or stay invite-link only.
- Storage horizon defaults for the pruning processor (proposed: keep last 365 days + all of one's own contributions + any contribution one has affirmed, prune everything else).

**Risks.**

- p2panda 0.6 group-encryption primitives are newer / less battle-tested than the rest of the stack — expect to find rough edges during Phase D.
- `p2panda-stream` is `!Send`; integrating it with relm4's async runtime might require a `LocalSet` and careful task scheduling. Validate this in Phase A before committing to the architecture.
- Causal ordering correctness is subtle. The CausalOrderingProcessor needs property-based tests (random op arrival orderings produce the same final state) before being trusted as the pipeline spine.

**Naming.** "Tier-0 / Tier-1" disappears as conceptual layers in the new architecture. There's just "the global gossip topic," "per-key topics," "pair topics," and "circle topics." Documentation should update accordingly.
