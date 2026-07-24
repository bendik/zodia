# PRD: Collaborative interpretations (replaces Phase F)

**Status:** needs-triage
**Branch:** TBD (will be `feat/collab-interps`)
**Foundation already landed:** Phases A–C-1 (0.7.0) and Phase E activity feed (shipping next as 0.8.0).
**Supersedes:** `docs/prd/circles-mvp.md` — the chat-circles model is replaced by this collaborative-document model.  Circles become a *derived* presence concept on top of editing sessions rather than a primary feature.

## Problem Statement

Zodia today treats community contribution as **competing whole interpretations**: each peer authors a complete prose blurb under an aspect key; affirmations rank them; responses thread off them.  This is a tweets-style model and it has shipped well enough for 0.7.0, but it hits a ceiling fast:

- **Knowledge stays shallow.** A single interpretation can't be improved; if a contributor sees a small flaw or wants to extend a phrasing, their only option is to author an entirely new competing version.  Communities don't *accrete* understanding this way — they fragment it.
- **Voting is too coarse.** A ♡ on a 500-character text says "I like all of this" — even when the reader actually only loves one sentence.  Granular signal is lost.
- **Responses don't compose.** Threaded responses are siblings, not edits.  A great response to a flawed parent never gets to refine the parent itself.
- **Provenance erodes.** When five peers have written near-identical interpretations of "Venus square Pluto," the first author's framing is functionally invisible; community attention scatters.
- **The "circle" we drew up for Phase F was the wrong primitive.** A chat room about an aspect is a fine product, but it's *parallel* to the interpretations rather than directly improving them.  The thing users actually want to do together is *write the meaning of a chart together*.

## Solution

Each `InterpKey` becomes a **single collaboratively-edited document** instead of a list of competing entries.  Concretely:

1. **One document per key.** `natal:venus_square_pluto` has exactly one community body of text on the network — a CRDT-backed block-structured document.  Anyone can edit; convergence is automatic via the CRDT.
2. **Author-veto ring.** Each text block remembers the last `N` authors to have edited it.  Any one of those ring authors can issue a **Veto** op within a **7-day window** after a subsequent edit lands; veto auto-reverts that edit.  Beyond the window or the ring, text settles into canon.
3. **Notifications when your edit is edited.** A peer modifying a block you authored fires a "your block was edited" event, surfaced in the Sky feed and badging the bell.  The notification carries a link to the new revision and a quick veto affordance.
4. **Circles as derived presence.** When N≥2 peers are actively editing the same key's document, the editor session *itself* is the circle — UI surfaces "Alice and Bob are editing Venus-square-Pluto right now" with a join button.  No separate "circle" object needs to exist as wire format.
5. **Optional live audio in editor sessions.** When a circle has formed around an editor, members can opt in to a live audio channel attached to the session — the chart-discussion call we kept calling for.
6. **Affirmations re-targeted.** ♡ no longer affirms a "row" — it affirms a specific **revision** of the doc.  The community ranking signal for a key is "how many peers have affirmed the current revision."  Older affirmations become history on past revisions.
7. **Baseline text is anonymous canon.** Bundled TOML interpretations seed each key's document as un-vetoable initial text.  Migrated existing local-authored interpretations become the first community-block beneath it, authored by the original writer.

Resulting community body: per-key, one converged text everyone reads, edited live by anyone, with a soft veto layer protecting recent authors' contributions, and audio/presence emerging naturally where collaboration is active.

## User Stories

1. As a Zodia user reading an aspect page, I want to see a single living interpretation that everyone has been refining, so I'm reading what the community has converged on instead of comparing five drafts.

2. As a Zodia user with an idea, I want to edit the community interpretation directly — fix a phrasing, add a paragraph, restructure — without authoring a new competing version.

3. As a Zodia user whose edit was just modified, I want a notification so I can review what changed; if the edit damaged my meaning I want a one-tap veto that reverts it within 7 days.

4. As a Zodia user editing a key right now, I want my presence advertised to other peers reading that key so they can join the editing session and we can collaborate.

5. As a Zodia user joining an active editing session, I want optional live audio to discuss what we're writing together.

6. As a Zodia user reading a baseline interpretation that no one has edited, I want to see "baseline reading — be the first to refine it" so I know it's not community-curated yet.

7. As a Zodia user whose veto window has elapsed, I want to understand that this paragraph has now graduated to canon and I can't unilaterally revert it any more.

8. As a Zodia user who authored a paragraph two years ago, I want to know my words have long since become community text — I'm no longer in the ring buffer and don't get vetoed-edit notifications for it.

9. As a Zodia user affirming an interpretation, I want my ♡ to attach to *this revision* of the doc, so the ranking signal reflects whether the community-as-of-today resonates with my taste, not what was true two months ago.

10. As a Zodia migrating from 0.7.x, I want my previously-authored interpretations preserved as the first community-blocks beneath baseline text, with my authorship intact and full veto rights as the original author.

11. As a future Phase G/H/I implementer, I want the editor surface to expose a clean "presence + audio capability" hook so the chart-visualization, real-time presence, and audio-mesh layers all attach without rework.

## Implementation Decisions

### Document model

Each key's doc is a sequence of **blocks**.  A block is a CRDT-backed text region with an associated `block_id` (random 16-byte id at block creation) and a `ring_buffer: VecDeque<(VerifyingKey, edit_op_id)>` of the last `N=5` edits' authors.

We adopt **Loro** (https://loro.dev) as the text CRDT — most mature Rust-native option, supports rich-text + block-list semantics, has a stable serialisation format, and its update wire format is small enough to fit comfortably in a p2panda op body.  An alternative (`yrs` — Yjs port) was considered; Loro's first-class block model wins for our paragraph-veto unit.

Block-level granularity (not character-level) for the veto-ring because:
- A whole paragraph is the human-attention unit; per-character veto would be absurd.
- Attribution scales: each block carries ~5 authorship entries, total memory bounded.
- The CRDT operates at character granularity internally; the veto layer reasons about blocks.

### Op model

A new sibling enum `DocOp` lives alongside `InterpOp` in `zodia-ops`:

```rust
pub enum DocOp {
    /// One CRDT update against a key's doc.  `base_rev` is the doc-version
    /// hash the editor saw locally; convergence handles divergence.
    Edit {
        interp_key:      String,
        base_rev:        Hash,
        crdt_update:     Vec<u8>,    // Loro update bytes
        affected_blocks: Vec<[u8; 16]>,
    },
    /// Veto a specific `Edit` op.  Honoured iff:
    ///   1. revoker's pubkey is in the ring of at least one affected block,
    ///   2. veto issued within `VETO_WINDOW_DAYS = 7` of the edit's
    ///      header timestamp,
    ///   3. no later edit on the same block has already landed (vetoes
    ///      only roll back the most recent contribution).
    Veto {
        target_edit_op_id: Hash,
    },
    /// Affirm a specific revision of a key's doc.  Replaces the legacy
    /// "affirm one of many interpretations" model — the target_rev is a
    /// doc-version hash.
    AffirmRev {
        interp_key:  String,
        target_rev:  Hash,
    },
}
```

The legacy `InterpOp::{Author, Affirm, RespondTo}` variants are **deprecated** but still decoded for backwards-compat reads.  New writes use `DocOp` exclusively.

### Ring buffer + veto window constants

```rust
pub const RING_SIZE: usize = 5;
pub const VETO_WINDOW_DAYS: u64 = 7;
```

Both compile-time constants in `zodia-ops`.  Adjusting them is a wire-format-affecting change; bump version when changing.

### Auto-rollback semantics

When a `Veto` op is materialised:
1. Pipeline verifies (in the new `VetoAuthorityProcessor`) that the revoker is in at least one affected block's ring and within the time window.
2. If authorised: the CRDT generates a *compensating update* that re-applies the pre-edit state of the affected blocks (Loro's `OpLog::undo` shape).
3. The compensating update is **emitted as a new** `DocOp::Edit` from a synthetic "system" key so the rollback itself is a CRDT-legal op that everyone converges on.
4. Notification fires: original editor sees "X vetoed your edit on Venus square Pluto — open the editor to see why."

Vetoes don't "destroy" the rolled-back text from history; the doc's full edit log retains it, viewable via a future "history" UI.

### Notifications: "your edit was edited"

When `DocOp::Edit` lands and its `affected_blocks` overlap blocks containing any ring entry for the local identity, emit a `StateEvent::BlockYouAuthoredWasEdited { interp_key, edit_op_id, by }`.  Phase E feed renderer adds a new `FeedItem::BlockEdited` variant — bell-targeting, high salience.  The card has a quick-veto button (when within window).

### Pipeline integration

`zodia-pipeline` gains:
- A new `DocStateProcessor` that consumes `DocOp::Edit` and produces:
  - `StateEvent::DocEdited { interp_key, by, affected_blocks, new_rev }`
  - `StateEvent::BlockYouAuthoredWasEdited { ... }` (derived)
  - `StateEvent::DocRolledBack { interp_key, by, original_edit_op_id }` (when a Veto applies)
- A new `VetoAuthorityProcessor` that gates `DocOp::Veto` against ring membership + time window.
- The existing `CausalOrderingProcessor` continues to handle parent-link ordering for the legacy `RespondTo` ops; new ops don't need it (Loro handles ordering internally).

### Storage schema delta

```sql
-- Per-key collaborative doc, stored as a Loro snapshot.  Snapshots are
-- materialised on every N edits to bound replay cost; in between, the
-- snapshot + pending edit log are replayed on load.
CREATE TABLE interp_docs (
    interp_key       TEXT PRIMARY KEY NOT NULL,
    loro_snapshot    BLOB NOT NULL,
    snapshot_rev     BLOB NOT NULL,
    updated_at       INTEGER NOT NULL
) STRICT;

-- Per-block authorship ring.  Updated on every edit-op materialisation.
CREATE TABLE doc_block_authors (
    interp_key   TEXT NOT NULL,
    block_id     BLOB NOT NULL,                 -- 16-byte block id
    position     INTEGER NOT NULL,              -- 0..RING_SIZE-1, FIFO order
    author_pk    BLOB NOT NULL,
    edit_op_id   BLOB NOT NULL,                 -- the edit that put them here
    edited_at    INTEGER NOT NULL,
    PRIMARY KEY (interp_key, block_id, position)
) STRICT;

CREATE INDEX doc_block_authors_by_author
    ON doc_block_authors(author_pk);

-- Affirms now target doc revisions (not log_ids).  Sybil-resistant per
-- (rev, voter) like before.
CREATE TABLE doc_affirms (
    interp_key  TEXT NOT NULL,
    target_rev  BLOB NOT NULL,
    voter_pk    BLOB NOT NULL,
    affirmed_at INTEGER NOT NULL,
    PRIMARY KEY (interp_key, target_rev, voter_pk)
) STRICT;
```

The existing `interpretations` + `affirmations` tables stay for read-side compat during migration; new writes go to `interp_docs` + `doc_affirms`.

### p2panda-auth for capability gating

For Phase F-collab we use `p2panda-auth` minimally:
- Each interp_key doc has a degenerate "open" group: any p2panda identity is a member, anyone can write.
- The auth CRDT is still wired in so a future Phase J (private dedications) can install role-gated docs on UserChart anchors without rewriting the op layer.

For Phase F shipping: no UI exposure of roles.  Everything is open-edit.  The infrastructure is there to lock down later.

### Circles as derived presence

A "circle" is not its own wire-format entity in this PRD.  When the local node is editing a key, it broadcasts a lightweight presence op (`DocOp::EditorPresence { interp_key, joined: true }`) on the per-key gossip topic.  Other nodes subscribed to that topic see the presence stream; when N≥2 are simultaneously present, the UI surfaces "active editor circle."  Departure is a `joined: false` op or a presence timeout (`PRESENCE_TIMEOUT_SECONDS = 90`).

The "Start a Circle" CTA the user originally proposed becomes: **a "Join editing session" button** on the aspect detail page.  Clicking it puts you in the editor view and announces presence — whether you're alone or with others.

### Live audio attachment

When two or more peers are present on the same key's editor, a `Capability::EditorAudio { interp_key }` becomes negotiable through the pair-channel stream layer (Phase H/I work).  Phase F-collab ships *without* the audio capability wired up — the slot is reserved, audio lands later.

### Sky feed event vocabulary

Phase E's `FeedView` learns new variants:
- `FeedItem::DocEdited` — ambient signal that a key you care about saw an edit.
- `FeedItem::BlockYouAuthoredWasEdited` — bell-targeting, with quick-veto action.
- `FeedItem::DocRolledBack` — informational: "X vetoed an edit, doc reverted."
- `FeedItem::EditorPresenceJoined` — "Alice is editing Venus-square-Pluto right now."  Surfaces only when local user is also subscribed to the key or it's in their chart.
- `FeedItem::DocAffirmed` — replaces `AffirmAdded` for new ops.

The old `FeedPayload::InterpAuthored / AffirmAdded / ResponseAdded` are retained for backwards-compat rendering of pre-0.9 ops but not produced by new flows.

### Migration of existing data

On first 0.9 launch the app runs a one-time migration:
1. For each distinct `interp_key` in `interpretations`, create an `interp_docs` row with a fresh Loro doc.
2. Seed the doc:
   - First block: bundled-baseline text (anonymous, un-vetoable).
   - One block per existing local-authored row, in `received_at` order, attributed to the original `author_pk` (gets a ring entry).
3. For each row's affirmations, port them to `doc_affirms` against the *current* (post-migration) revision hash.  This is intentionally imperfect — affirms become "the community as of now" rather than retroactive.
4. Legacy `interpretations` table is preserved for read-back UI showing old-format thread history; not used for new writes.

Migration is idempotent (keyed by interp_key existence in `interp_docs`).

### Editor UI

A new `app/src/doc_editor.rs` component:
- Adwaita-flavoured rich-text editor backed by Loro.
- Block-aware: paragraphs are visually distinct, hover shows current ring (small avatar stack of recent editors).
- Inline indicator when another peer is currently editing this block (purple cursor flicker, peer-tag tooltip).
- Toolbar: "veto last edit on this block" (visible only if within window + ring), "affirm current revision" (the new ♡), "publish my edit" (commits pending CRDT updates as one `DocOp::Edit`).
- Replaces the existing interpretation-list body inside `aspect_view::detail_page` when a doc is present.  Baseline-only docs (no community edits yet) show "Be the first to refine the community reading" with an Edit button → enters editor.

### What changes in existing code

- `zodia-ops`: new `DocOp` enum + tests.  Deprecation comments on legacy variants.
- `zodia-pipeline`: new `DocStateProcessor` + `VetoAuthorityProcessor` + new `StateEvent` variants.
- `zodia-store`: new tables (above) + helpers (`doc_load`, `doc_apply_edit`, `doc_affirm_rev`, `block_ring`).
- New crate `zodia-doc`: Loro wrapper + block-ring management + p2panda-auth glue.  Thin facade; doesn't bake in Loro everywhere.
- `app/`:
  - `aspect_view::detail_page`: when `interp_docs` row exists, render via the new editor component instead of the legacy InterpRow factory.
  - New `doc_editor.rs` (rich-text + block ring UI).
  - `feed_view.rs`: 5 new `FeedItem` variants + rendering.
  - `app.rs`: new `AppMsg` variants — `EditDoc`, `PublishEdit`, `VetoEdit`, `AffirmRev`, `JoinEditor`, `LeaveEditor`.  Many legacy `AppMsg` variants (`SubmitInterp`, `SubmitResponse`, `AffirmInterp`) get marked deprecated.

## Testing Decisions

- **`zodia-ops` codec.** Round-trip ser/de for every `DocOp` variant.  Forward-compat on unknown extra fields.
- **Ring buffer maintenance.** Property tests: applying N+1 sequential edits to one block leaves a ring of exactly the last N authors in FIFO order; the oldest author falls off cleanly.
- **Veto authority.** Tests for:
  - Veto from ring author within window → applies.
  - Veto from non-ring author → drops.
  - Veto from ring author after window → drops.
  - Veto when a *later* edit already landed → drops (compensation is the later edit's editor's problem to negotiate).
- **CRDT convergence under partition.** Two `zodia-doc` instances edit the same block offline; merge converges to the same state on both, and ring buffer ends up consistent.
- **Migration correctness.** Fixture with N existing `interpretations` rows → assert `interp_docs` has one doc per key, blocks include baseline + each authored row, ring populated correctly, affirms ported.
- **Editor UI integration.** Smoke tests around the new editor component using a mock store + in-memory Loro instances.

We continue to skip live-iroh integration tests at the Zodia layer.

## Out of Scope

- **Audio attached to editor sessions.** Capability slot is reserved; the implementation (mesh audio + recording + replay) is Phase I work.
- **Private docs on UserChart anchors.** Phase J (private dedications) — uses `p2panda-auth` roles to gate writers.  Phase F-collab ships open-edit only.
- **Per-character cursor sharing (Google-Docs style).** Block-level presence flicker is all we ship in Phase F-collab.  Per-char cursor presence is a polish pass.
- **Rich-text formatting (bold/italic/links).** Plain text initially.  Loro supports rich text; we just don't expose the formatting toolbar in this phase.
- **History UI showing full edit log.** Backend retains it (Loro keeps the op log); UI surface is Phase F+1.
- **Cross-key search.** Out of scope.
- **Federated moderation beyond the veto ring.** Mute / block / reputation are their own future design.
- **Pruning of old doc edit ops.** Storage retention for the Loro log is Phase K (pruning).
- **Migration of `RespondTo` threads.** Existing responses stay rendered in the legacy view; they don't fold into the new doc.  A future PR may surface them as inline annotations on doc blocks.

## Further Notes

**Version cut.** Phase F-collab adds wire-format-incompatible ops.  Peers on 0.8.x see `DocOp::*` and drop them at decode (`Skipped::MalformedOp`).  Authoring an edit on the new doc model produces no community read for sub-0.9 peers.  Ships as **0.9.0** with a known-incompat note: the community body diverges across the 0.9 cut and only converges once everyone upgrades.  Acceptable trade for the model shift.

**Why this replaces the original Phase F.** The original PRD's chat-circles framed circles as parallel rooms about a topic; users get to discuss but the *artifact* (interpretations) stays unchanged.  This PRD makes the artifact itself the collaboration surface — the circle is the *act* of editing together, not a separate room.  Net result: one feature ships instead of two (interpretations + circles), and they're the same feature.

**Phasing context (revised).**

| Phase | What | Status |
|---|---|---|
| A | `zodia-ops` + `zodia-pipeline` scaffolding | shipped (0.7.0) |
| B | Network-replicated affirmations + sync metrics | shipped (0.7.0) |
| C-1 | Causal response threads | shipped (0.7.0) |
| E | Activity feed | next ship (0.8.0) |
| **F-collab** | **Collaborative interpretations (this PRD)** | **follows E (0.9.0)** |
| G | Chart visualization component | follows F-collab |
| H | Pair-channel stream rework + presence | follows G |
| I | Mesh audio (≤6) + recording + replay attached to editor sessions | follows H |
| J | Private/role-gated docs on UserChart anchors via p2panda-auth roles | follows I |
| K | Pruning processor + retention | follows J |
| C-2 | Lazy per-key topic subscription | interleaves where convenient |

**Open questions to resolve during implementation.**

- Exact Loro version + serialisation stability guarantees — Loro is pre-1.0; we pin a version and accept the migration cost on Loro bumps.
- Snapshot-every-N-edits threshold — start with N=20; tune based on op-log replay cost.
- "Affirm revision" UX when revisions are constantly churning — probably affirm-the-revision-at-time-of-tap; visible as "you ♡'d an older revision; affirm current?" prompt later.
- Whether `DocOp::EditorPresence` flows on the same per-key topic as edits, or a separate "presence" topic.  Same topic is simpler; separate topic lets non-editors filter presence noise.
- The block-id scheme — random 16-byte at creation is fine, but should they survive block splits?  Loro semantics here need pinning.

**Risks.**

- **Loro maturity.** Pre-1.0 library, evolving wire format.  Mitigation: pin a version, isolate Loro behind the `zodia-doc` crate so swapping CRDTs later is a single-crate concern.
- **CRDT op-log growth.** Each edit appends to Loro's internal log indefinitely.  Bounded by snapshotting + Phase K pruning, but not free.
- **Veto auto-rollback as a social weapon.** A ring author can revert anyone's edit silently within 7 days.  If abused it becomes "the most recent contributor wins forever."  Mitigation in this PRD is the rolling window: veto rights themselves age out.  Future moderation tooling (Phase J+) may add report-on-veto.
- **Convergence ambiguity around veto + further edit.** Spec says a later edit on the same block invalidates veto.  Need to test the race: edit lands → veto issued → another edit lands → which wins?  Test exhaustively in `zodia-doc`.
- **First-run migration is heavy** for users with large `interpretations` rows.  Run on a background task with a "migrating community library…" splash; idempotent on retry.

**Naming.** User-facing copy says "the community reading" or "this aspect's reading" — not "doc" or "interpretation."  Internal types stay `interp_doc` / `DocOp` for code readability.  The "circle" word continues to surface in UI only when ≥2 peers are present in an editor session.
