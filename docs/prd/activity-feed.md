# PRD: Activity feed (Phase E)

**Status:** shipped (0.7.1)
**Branch:** `main`
**Foundation already landed:** 0.7.0 (Phases A + B + C-1 of the operations-and-streams rearchitecture — `zodia-ops`, `zodia-pipeline`, network-replicated affirmations, causal response threads, LogSync session lifecycle in the Network tab)

## Problem Statement

Phases B and C-1 of the operations-and-streams rearchitecture have already shipped the *data* for network-wide affirmations and causal response threads, but neither is visible in the UI. Concretely:

- **Affirmations on your work are silent.** When another peer affirms an interpretation you authored, the op arrives, the pipeline materialises it, the count updates in the database — and nothing surfaces to you. The "your contribution was seen" moment never lands.
- **Threaded responses are unrendered.** `InterpOp::RespondTo` lands in the store as a causal child of its parent, but there is no UI that shows the thread; responses are written but unread by design.
- **The Sky tab is a dense static table.** Sky shows the user's 22 active transit aspects as a click-heavy list. Each row is a single line; the page is dominated by repetition and requires a tap-per-aspect to learn anything.
- **Per-aspect pages are flat lists.** `aspect_view.rs` renders a list of community interpretations with affirm/respond buttons. Activity that arrives after the page opens isn't reflected; there is no sense of "what's been happening here recently."
- **The notification bell exists but never badges.** `notif_bell.rs` is a working widget with no signal flowing into it — there is no read-state, no notion of "events about you," and no event taxonomy.

The result is that Zodia 0.7.0 *has* a community body but doesn't *feel* like one. The infrastructure investment of Phases A–C-1 sits below a UI that hasn't caught up.

## Solution

Unify the Sky tab and the per-aspect pages around a single time-ordered feed renderer. Op-events that already flow through the pipeline (`InterpAuthored`, `AffirmAdded`, `ResponseAdded`) become first-class feed items alongside locally-computed transit-in-orb events. The Sky tab becomes a chronological interleave of "what's happening in the sky right now" and "what's happening in the community right now," sharing one visual language. Per-aspect pages reuse the same renderer with a key filter so an aspect's page is a live view of *that aspect's* activity, not a frozen list.

The notification bell gains a real signal: a badge counts events that *target the current user* — affirmations of your interpretations, responses to threads you authored. Ambient activity contributes to feed freshness but does not interrupt.

No new wire format. No new ops. No migration. The entire phase is UI work on the relm4 side plus one small `feed_read` table in `zodia-store` for per-event read tracking. This phase cashes in the already-merged work of Phases B and C-1, and sets a renderer foundation that Phase F (circles MVP) can plug new event variants into.

## User Stories

1. As a Zodia user who has authored an interpretation, I want to see when other peers affirm it, so my contribution feels seen rather than disappearing into the void.

2. As a Zodia user who has authored an interpretation, I want responses from other peers to surface in my feed grouped under my original, so I can follow conversations on my work.

3. As a Zodia user opening the app, I want one home view that interleaves what's happening in the sky right now with what's happening in the community right now, so the dense transit table doesn't dominate the experience.

4. As a Zodia user, I want transit aspects entering and leaving orb to show up as feed items, so I can notice astrological moments without having to scan a static table.

5. As a Zodia user, I want a notification bell that badges only for events about my own work, so I can trust it as a real signal rather than ambient chatter.

6. As a Zodia user, I want to mark a feed card as unread after I've already viewed it, so I can come back to something later.

7. As a Zodia user visiting an aspect's page, I want to see recent affirmations and responses on that aspect, so the page feels alive instead of being a static interpretations list.

8. As a Zodia user, I want unread items in the feed to be visually distinct from read ones, so I can scan quickly without re-reading what I've already processed.

9. As a Zodia user, I want clicking on a feed card to navigate to the relevant aspect page or contribution thread, so the feed acts as a navigation surface and not a dead end.

10. As a Zodia user reconnecting after an offline gap, I want events that arrived during the gap to surface in my feed and badge the bell, so I don't miss activity from the time I wasn't watching.

11. As a future Phase F (circles) implementer, I want one `FeedView` component that consumes a typed event stream, so adding new event types — circle-invite-arrived, hang-started, recording-published — is one new variant rather than a new UI surface.

12. As a Zodia user, I want the bell to clear its badge when I click it but to leave individual cards in the feed marked unread until I've actually viewed them, so dismissing the badge doesn't lose track of what I haven't read.

## Implementation Decisions

### Event taxonomy

A new `FeedItem` type, defined in `app/` (not in a shared crate — Phase E feed items don't cross process boundaries), is the union of two sources:

- **Pipeline `StateEvent`s** already emitted by `zodia-pipeline`:
  - `InterpAuthored` → `FeedItem::InterpAuthored`
  - `AffirmAdded` → `FeedItem::AffirmAdded`
  - `ResponseAdded` → `FeedItem::ResponseAdded`
  - `Skipped` events are *not* surfaced in the feed; they stay in tracing logs and the Network tab's existing observability.
- **Locally-computed transit ticks** from a new `TransitTicker` source:
  - `FeedItem::TransitEnteredOrb { transit_key, jdn }`
  - `FeedItem::TransitLeftOrb { transit_key, jdn }`

System events (LogSync session lifecycle, peer discovery, reconnect) continue to live in the Network tab; they are explicitly *out* of the Sky feed. Sky's job is "what's happening in the cosmos and the community right now"; Network's job is "how am I connected to other people." Folding plumbing telemetry into Sky muddles both.

Each `FeedItem` carries:

- `event_id: [u8; 32]` — for pipeline events this is the op hash; for transit events it is `blake3("transit" || transit_key || enter|leave || jdn_bucket)`, deterministic across restarts so a tick at 14:00 generates the same id on every device.
- `timestamp: i64` — milliseconds; from the op header for pipeline events, from the JDN for transit events.
- `targets_me: bool` — derived locally: `true` iff the event references content the local identity authored (an affirmation on my interp, a response to my thread). Transit events never target.
- `payload` — variant-specific data needed to render the card.

### The `TransitTicker` source

A new local-only event source under `app/` (not a pipeline processor — transit events are not ops and shouldn't pass through op-shaped infrastructure). It:

- Wakes on a tokio interval (default 10 minutes) and on user navigation to Sky.
- Computes current transit aspects against the local identity's natal chart via `zodia-core::transit`.
- Diffs against the previous tick's in-orb set: aspects newly in orb emit `TransitEnteredOrb`; aspects newly out of orb emit `TransitLeftOrb`.
- Persists the previous tick's set across restarts in a single-row `feed_meta` entry so a restart doesn't re-emit every active transit.
- Emits into the same `mpsc::Sender<FeedItem>` the pipeline writes to. Order at the receiving end is by `timestamp`, so a transit tick interleaves correctly with op events sorted by their original header timestamp.

The deterministic `event_id` (above) means that if the user is offline through an enter→leave cycle and only comes back after both have happened, both events still land in the feed at their correct timestamps and neither double-counts on re-tick.

### The `FeedView` widget

One reusable relm4 component under `app/src/feed_view.rs`. Construction takes a filter predicate:

- Sky tab: `|_| true` — every item shows.
- Per-aspect page: `|item| item.interp_key() == Some(this_key)`.

Internally backed by a `FactoryVecDeque<FeedCard>` for virtualisation; the feed can grow to hundreds of items without paint cost.

Card layout is per-variant. Each card includes:

- Variant-specific glyph and accent (♡ for affirm, ↳ for response, ✎ for author, ◐/◑ for transit enter/leave).
- Timestamp rendered as relative ("3m ago", "yesterday") with absolute on hover.
- Author name / pubkey-tag where applicable.
- A read/unread visual state (subtle accent dot when unread).
- Click → navigates to the relevant aspect page and, where applicable, scrolls to the relevant op.
- Long-press / context menu → "Mark unread" / "Mark read." Auto-marks read after the card has been on screen for ≥1.5s (intersection observer pattern via `gtk::Adjustment` watching).

The same component is what Phase F will extend with `FeedItem::CircleInviteArrived`, `FeedItem::HangStarted`, etc.

### Sky tab restructure

`aspect_list.rs` stops being the Sky tab content. The Sky tab becomes a `ToolbarView` containing a single `FeedView` with the unfiltered predicate. The current transit-aspect table is no longer rendered as a separate section — its content flows in as `TransitEnteredOrb` / `TransitLeftOrb` cards interleaved with op events.

`aspect_list.rs` itself is retained and reused on the Chart tab (which is still the static "your natal placements" view) so the existing code does not need to be deleted, only unmounted from Sky.

### Per-aspect page restructure

`aspect_view.rs` keeps its title and metadata header, but the body — currently a list of community interpretations with affirm/respond buttons — is replaced by a `FeedView` filtered to events whose `interp_key` matches this aspect. The affirm/respond buttons remain accessible via a card-level action on each `InterpAuthored` feed card (the existing per-interp logic is reused; only its container changes).

### Notification bell

`notif_bell.rs` gains a badge count that is the query:

```sql
SELECT COUNT(*) FROM events_targeting_me_view WHERE event_id NOT IN (SELECT event_id FROM feed_read);
```

`events_targeting_me_view` is a SQL view over the existing op tables — joining `interp_received` against the local identity for affirmations and responses. The bell subscribes to a relm4 signal raised whenever a `FeedItem` with `targets_me = true` arrives or whenever `feed_read` is mutated.

Clicking the bell:

1. Navigates to the Sky tab.
2. Inserts `feed_read` rows for all currently-pending targeting events at once (bulk acknowledge).
3. Does not mark non-targeting events as read — those follow the per-card auto-mark-read rule.

This matches the contract: the bell is the bulk-acknowledge surface for stuff about you; per-card read state is finer grained and serves the "save for later" pattern.

### Storage schema delta

Two new tables in `zodia-store`, both small and bounded:

```sql
CREATE TABLE feed_read (
    event_id BLOB PRIMARY KEY NOT NULL,
    read_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE feed_meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) STRICT;
-- known keys:
--   'transit_in_orb_set' → CBOR-encoded Vec<TransitKey>
```

No existing tables change. The `targets_me` predicate is computed at materialisation time (in `zodia-pipeline` or by querying `interp_received` joined against the local identity) and does not require a schema change.

### Pipeline integration

`zodia-pipeline` is unchanged in this phase. The relm4 layer that today consumes `StateEvent` and updates the legacy refresh-token-bumped UI is rewritten to instead push each `StateEvent` into the `FeedItem` channel, with the `TransitTicker` pushing into the same channel. The aggregator drains the channel, sorts by timestamp on insertion into the `FactoryVecDeque`, and computes `targets_me` by joining the event against the local identity.

### What disappears

- The `network_changed_token` bump that drives Sky refreshes today.
- The Sky-tab variant of the aspect list (the Chart tab retains it).
- The dead `notif_bell` placeholder logic.

The `AppMsg` variants that today route community contributions (`InterpReceived`, `SyncInterpReceived`, etc.) collapse into a single `FeedItemReceived(FeedItem)` variant.

## Testing Decisions

Phase E is the first phase where the work is mostly UI rather than pipeline / wire format, so the test surface is shaped differently from Phases A–C-1.

- **`FeedItem` ordering and dedup.** Unit tests against an in-memory `FactoryVecDeque` shim: insert a known sequence of pipeline events + transit ticks in scrambled order; assert the final feed is timestamp-sorted, contains no duplicate `event_id`s, and is stable across re-insertion of the same event.
- **`TransitTicker` enter/leave correctness.** Property-style test: generate random sequences of (jdn, in-orb-set) pairs; assert that exactly the set-difference is emitted as `TransitEnteredOrb` and the inverse difference as `TransitLeftOrb`. Round-trip test: enter then leave produces exactly two events with stable ids.
- **Bell badge count.** Integration test against the SQLite store: insert N op events targeting the local identity, K not, M already in `feed_read`. Assert badge query returns `N - M`. Then click-bell, assert all N targeting events now in `feed_read` and badge is 0.
- **`FeedView` filter predicate.** Construct a `FeedView` with a key filter, feed it a mixed stream of events for that key and other keys, assert only matching events render.
- **Read-state lifecycle.** Auto-mark-read fires only after the dwell threshold (≥1.5s); manual mark-unread persists; restart preserves both.

No end-to-end test against live iroh networking is added in Phase E — `StateEvent` ingest is already exercised by Phase A's wire-up tests, and the new code is downstream of that.

## Out of Scope

- **Circles.** Typed-anchor circles, public/private split, sidebar circles list, hangs presence, audio, recording, replay — all Phase F and onward. Phase E does *not* introduce any `FeedItem` variant for circle events; that's the first thing Phase F adds.
- **Chart visualization.** Cairo-rendered natal wheel — Phase G. Phase E continues to render aspect placements textually wherever it needs to.
- **Lazy per-key topic subscription** (the original C-2 work). Phase E reuses the current eager subscription model. Per-aspect pages still subscribe to their key topic eagerly when opened; lifecycle work moves to a later phase.
- **Pruning processor.** Storage retention is out of scope; `feed_read` grows monotonically until pruning lands. At expected event volumes (thousands of events / year per active user) this is fine for Phase E's lifetime.
- **Pair-channel stream rework.** Phase D-channels (dropping the ALPN, capability negotiation) stays untouched.
- **Encryption.** No new encrypted content lands in Phase E. Private UserChart circles and group-key rotation are Phase K work.
- **Mobile-width layout polish.** The `FeedView` should *function* at narrow widths but visual design pass for compact form factor is deferred. The reusable-component constraint from the chart-viz discussion applies more to Phase G.
- **Cross-device sync of read-state.** `feed_read` is per-device. Reading a card on one device does not mark it read on another. Synced read-state is a future identity feature.
- **Activity feed for events from peers you've blocked / muted.** No mute/block primitive exists in 0.7.0; out of scope here.

## Further Notes

**Why this phase is shippable as 0.8.0.** Phase E adds no new wire format and no new op variant. Every peer on 0.7.x continues to interoperate; the differences are entirely local to the upgrading device. The version bump is justified by user-visible UX shift, not protocol change.

**Connection to the original PRD.** The operations-and-streams rearchitecture PRD listed Phases A–D; Phase E sits *between* C-1 (which shipped in 0.7.0) and the remaining C-2 / D work. It does not change that PRD's commitments; it adds a UI-layer phase that exists primarily to cash in B and C-1's already-merged data.

**Phasing context (post-grilling sequence).**

| Phase | What | Status |
|---|---|---|
| A | `zodia-ops` + `zodia-pipeline` scaffolding | shipped (0.7.0) |
| B | Network-replicated affirmations + sync metrics | shipped (0.7.0) |
| C-1 | Causal response threads | shipped (0.7.0) |
| **E** | **Activity feed (this PRD)** | **next** |
| F | Circles MVP — typed anchors, public-only, text chat, sidebar list, Sky surfacing | follows E |
| G | Chart visualization component (Cairo wheel, reusable) | follows F |
| H | Pair-channel stream rework + hangs presence as a capability | follows G |
| I | Mesh audio (≤6) + per-participant recording + replay | follows H |
| J | Private UserChart circles + group encryption + chart key export/import | follows I |
| K | Pruning processor + retention policy UI | follows J |
| C-2 | Lazy per-key topic subscription | interleaves where convenient; not a hard blocker for any later phase |

**Open questions to resolve during implementation.**

- Exact dwell threshold for auto-mark-read (proposed 1.5s — UX feel, adjustable in a setting).
- Whether transit ticks fire on `Sky` tab focus (in addition to the 10-minute interval) — probably yes, so opening Sky after several hours doesn't surface a stale tick set.
- Whether `feed_read` rows should be persisted with a retention horizon (e.g. 90 days) — deferred to Phase K (pruning) rather than guessed now.
- Visual treatment of "an old event arrived via sync after a long offline gap" — should those badge the bell? Per the user stories above: yes. But ordering them at their original timestamp puts them deep in the scroll, where the bell badge will *find* them but the user might not. Probably surface a single "N older events arrived" summary card at the top on app open after a long offline gap. Decide during UI prototyping.

**Risks.**

- The `FactoryVecDeque<FeedCard>` may need explicit windowing for users with many years of accumulated events. Validate render perf at 5k items during Phase E's QA pass; if too slow, add a "last 30 days" default window with "load more" affordance.
- Auto-mark-read via intersection-observer-style behaviour is GTK-idiomatic but not battle-tested in this codebase. Phase E is the first place we need it. If it proves brittle, fall back to "all cards visible at the moment of a sentinel scroll-stop" — coarser but reliable.
- The bell badge query touches `feed_read` and a view over the op tables. If query latency at scale becomes visible, add an indexed denormalised counter table updated by the same writer that mutates `feed_read`.

**Naming.** The Sky tab keeps its name. The feed is consistently called "the feed" in user-facing copy, not "timeline" or "stream"; "stream" is reserved for the technical pair-channel work in Phase H.
