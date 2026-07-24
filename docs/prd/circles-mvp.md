# PRD: Circles MVP (Phase F) — SUPERSEDED

> **This PRD is superseded by `collaborative-interpretations.md`.**
>
> The chat-circles model framed here is replaced by the collaborative-document
> model documented in that PRD: each `InterpKey` becomes one collaboratively
> edited text with an author-veto ring, and "circle" becomes a *derived*
> presence concept attached to active editor sessions rather than a separate
> first-class wire entity.  The shift was made during Phase E shipping after
> recognising that competing-whole-interpretations doesn't scale to depth
> and that p2panda's CRDT primitives let us model a single converging
> community body of text per aspect.
>
> Kept on disk as historical record of the chat-circles design alternative.

**Status:** superseded by collaborative-interpretations.md
**Branch:** TBD (will be `feat/circles-mvp`)
**Foundation already landed:** Phases A–C-1 (operations-and-streams rearchitecture: `zodia-ops`, `zodia-pipeline`, network-replicated affirmations, causal response threads — shipped 0.7.0).
**Foundation assumed shipping immediately before:** Phase E activity feed — Sky becomes a chronological `FeedView`, per-aspect pages reuse the same renderer, notification bell badges events targeting the local user.

## Problem Statement

Zodia's community surface today is shaped around *contributions* (interpretations on a fixed set of astrological keys) and *pairs* (two-peer consent channels). There is no first-class affordance for **a small group of peers gathering around a shared topic**. Specifically:

- **You can't "form a circle" around an aspect.** If three friends want to talk together about Venus square Pluto, the only option is one-to-one pair chats; the group concept doesn't exist in the wire format or the UI.
- **You can't leave a reading for someone.** A peer who wants to dedicate a chart reading to you — "here's what I see in your Venus" — has nowhere to do it. The closest analogue is authoring an interpretation on one of your aspect keys, which scatters the reading across many keys instead of being a single addressable artifact about *you*.
- **Topic-discovery for community gatherings doesn't exist.** Even if circles existed, there's no mechanism for finding the ones happening around keys you care about, around your own chart, or around free-form interests.
- **The sidebar treats peers as the only first-class social entity.** "Others" lists individual stargazers; there is no parallel list of groups you're part of.
- **The activity feed (Phase E) can render every kind of personal event but circle events.** Phase E established the `FeedView` renderer; circles are the proximate source of new event variants that make the feed feel alive beyond affirmations and threads.

The user-facing framing, in the grilling that produced this PRD: *the killer feature for the sense of community is highlighting active hangs/circles addressing a topic or key pertaining to you*. Phase F is the first incarnation of that feature — minimum text-chat substrate, with audio, recording, and private encryption deferred to later phases.

## Solution

Circles are persistent, typed-anchor, public-only chat rooms in Phase F. Each circle has:

- A **typed anchor**: `Anchor = AstroConcept(InterpKey) | UserChart(VerifyingKey) | FreeForm(String)`. The anchor is the load-bearing field for discovery and for Sky filtering.
- A **dedicated p2panda topic** carrying text-message ops authored by members.
- A **public visibility**: anyone can subscribe, anyone can post. No encryption, no membership gate. Phase J adds the private variant (UserChart-only, with the subject auto-joined as inseparable member).
- A **listed presence in the new "Circles" sidebar section**, sibling to "Others", visible only when the local user is a member of at least one circle.

Circles are discovered via a small **global circle-index topic** carrying lightweight `CircleAnnounce` ops. Every node subscribes to the index topic; local filtering decides which circles surface in Sky (anchor matches local user's chart or pubkey) and which surface in the Network tab (everything else, browsable). This trades some background bandwidth for radical simplicity — Phase F is an MVP, not a scaled deployment.

A **hang** in Phase F is a derived signal: "this circle has received ≥3 message ops in the last 10 minutes." That's enough to drive a pulse indicator on sidebar circle rows and an "active now" badge on Sky cards. Phase H upgrades the hang signal from "recent ops" to "real-time presence ops" once the pair-channel capability stream lands; the UI contract stays identical across the upgrade.

Phase F adds **no audio**, **no recording**, **no encryption**, **no chart visualization** — those are explicitly Phases G/H/I/J. The circle page in Phase F shows a textual anchor description where Phase G will later draw the natal wheel.

## User Stories

1. As a Zodia user, I want to create a circle anchored to a specific aspect (e.g. Venus square Pluto), so other peers interested in that aspect can find and join the conversation.

2. As a Zodia user, I want to create a circle anchored to another peer's chart, so I can leave them a dedicated reading other interested peers can join.

3. As a Zodia user, I want to create a free-form circle with a chosen title, so I can host a conversation that doesn't fit any specific astrological anchor.

4. As a Zodia user, I want any circle anchored to *my* chart to surface immediately in my Sky feed and badge my notification bell, so I find out the moment someone dedicates a reading to me.

5. As a Zodia user, I want circles anchored to aspects in *my* natal chart to surface in my Sky feed (without badging the bell), so I can opt into community conversations on keys I care about.

6. As a Zodia user, I want a "Circles" section in the sidebar listing every circle I've joined, so my groups feel as first-class as my pairs.

7. As a Zodia user, I want each sidebar circle row to show an unread-message counter, so I notice when a circle I'm in has new activity.

8. As a Zodia user, I want active circles (≥3 messages in the last 10 minutes) to display a pulse indicator, so I can tell at a glance which of my circles is "happening right now."

9. As a Zodia user, I want the Network tab to list every public circle on the network, filterable by anchor type, so I can browse and join circles I wouldn't otherwise see.

10. As a Zodia user, I want clicking a sidebar circle row to open the circle page, with its message history and an entry to send new messages, so the chat interaction is conventional and discoverable.

11. As a Zodia user, I want to leave a circle I'm no longer interested in, so the sidebar stays curated.

12. As a Zodia user, I want a "Report" action on any circle in the Network tab, so abusive circles can be flagged. (Phase F records reports locally; cross-network moderation is deferred.)

13. As a future Phase G/H/I/J implementer, I want circle ops, circle topics, and the `FeedView` integration to be shaped so that adding the chart-wheel backdrop, real-time presence, multi-party audio, recording, and the private UserChart variant are additive — no rework of the Phase F op model.

14. As a Zodia user opening the app after time offline, I want CircleAnnounce ops that arrived during my absence to surface in my Sky feed at their original timestamps, so I don't miss circles formed about me.

## Implementation Decisions

### Op model

A new sibling enum `CircleOp` lives alongside `InterpOp` in `zodia-ops`. CBOR-encoded; same forward-compat properties as `InterpOp` (extra map keys ignored on decode).

```rust
pub enum Anchor {
    AstroConcept(String),   // canonical interp_key, e.g. "natal:venus_square_pluto"
    UserChart(VerifyingKey),
    FreeForm(String),       // max 120 chars, validated at encode time
}

pub enum CircleOp {
    Message { body: String },     // text chat, max 2048 chars
    Join,                          // explicit membership claim (audit + sidebar)
    Leave,                         // explicit departure
}

pub enum CircleAnnounceOp {
    Created {
        circle_id: Hash,           // BLAKE3(creator_pk || anchor_cbor || created_at)
        anchor:    Anchor,
        title:     String,         // max 120 chars, optional display title
    },
    Closed { circle_id: Hash },   // creator-only; tombstone for sidebar / Network tab cleanup
}
```

**Two ops, two topic classes.** `CircleOp` flows on the per-circle topic; `CircleAnnounceOp` flows on the single global circle-index topic. The pipeline's `DecodeProcessor` branches on topic class (passed alongside the raw `Operation<()>`) to pick the right decoder.

**Why two enums.** Keeps each topic's wire format tight. The per-circle topic carries high-volume chat; the index topic carries low-volume metadata. Mixing them would force every chat message to be discriminated against every index variant on decode — wasteful and confusing.

**Why not extend `InterpOp`.** The name would no longer match the contents, and the decoder would conflate two semantically separate concerns. Pattern-matching code in the materialisation layer would grow a `_ => unreachable!()` arm for every cross-domain combination.

**Topic membership and the `Join` op.** Posting `CircleOp::Message` on a public circle topic is mechanically sufficient to participate — there's no ACL to gate the message. The `Join` op exists for **sidebar inclusion** (locally, the materialisation layer queries `joined_circles` and renders only those) and for **moderation audit** (so a Reporter can see who's claimed membership). Posting a message implicitly joins on first author; the explicit `Join` is for read-only members who want sidebar listing.

### Topic management

Three new topic derivations land in `zodia-core::topic`:

```rust
pub fn topic_circle(circle_id: Hash) -> TopicKey;
pub fn topic_circle_index() -> TopicKey;
```

- **Per-circle topic:** `blake3("zodia:v1:circle:" || circle_id)`. Subscribed by all current members; unsubscribed on `Leave` and on circle `Closed`.
- **Global circle-index topic:** `blake3("zodia:v1:circle-index")`. Every node subscribes; carries only `CircleAnnounceOp`. Sized: at expected MVP volumes (10s–100s of circles per day across the network) the bandwidth is negligible. A later phase introduces per-anchor index topics if scale demands.

**Eager subscription is fine in Phase F.** The lazy per-key topic subscription work (original C-2) is not a prerequisite — circles are explicitly joined, so subscription is naturally bounded by user action. The Network tab's "browse all circles" view subscribes to the global index only.

### Pipeline integration

`zodia-pipeline` gains two new `StateEvent` variants:

```rust
pub enum StateEvent {
    // ... existing variants ...
    CircleAnnounced {
        op_id:    Hash,
        creator:  VerifyingKey,
        circle_id: Hash,
        anchor:    Anchor,
        title:     String,
    },
    CircleMessage {
        op_id:    Hash,
        circle_id: Hash,
        author:   VerifyingKey,
        body:     String,
    },
    CircleMembershipChanged {
        circle_id: Hash,
        member:    VerifyingKey,
        joined:    bool,   // true on Join, false on Leave
    },
    CircleClosed {
        circle_id: Hash,
        by:        VerifyingKey,   // tombstone authority
    },
}
```

The `DecodeProcessor` takes a `topic_class` hint with each raw op (already available from the LogSync wire-up layer); per-circle topic ops decode as `CircleOp`, the index topic decodes as `CircleAnnounceOp`. The `MaterializationProcessor` translates these into the above `StateEvent`s with no causal-ordering requirements in Phase F (circle messages are flat — threading inside circles is a future feature).

### Schema delta

Three new tables in `zodia-store`:

```sql
CREATE TABLE circles (
    circle_id      BLOB PRIMARY KEY NOT NULL,
    creator_pk     BLOB NOT NULL,
    anchor_kind    INTEGER NOT NULL,   -- 0=AstroConcept, 1=UserChart, 2=FreeForm
    anchor_payload BLOB NOT NULL,      -- key string, pubkey bytes, or freeform string
    title          TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    closed_at      INTEGER             -- NULL while active
) STRICT;

CREATE INDEX circles_anchor ON circles(anchor_kind, anchor_payload);

CREATE TABLE circle_messages (
    op_id      BLOB PRIMARY KEY NOT NULL,
    circle_id  BLOB NOT NULL REFERENCES circles(circle_id),
    author_pk  BLOB NOT NULL,
    body       TEXT NOT NULL,
    sent_at    INTEGER NOT NULL
) STRICT;

CREATE INDEX circle_messages_by_circle ON circle_messages(circle_id, sent_at);

CREATE TABLE circle_membership (
    circle_id BLOB NOT NULL,
    member_pk BLOB NOT NULL,
    state     INTEGER NOT NULL,   -- 0=joined, 1=left
    changed_at INTEGER NOT NULL,
    PRIMARY KEY (circle_id, member_pk)
) STRICT;
```

Local-only:

```sql
CREATE TABLE circle_unread (
    circle_id     BLOB PRIMARY KEY NOT NULL,
    last_read_at  INTEGER NOT NULL   -- timestamp of latest CircleOp::Message marked read
) STRICT;

CREATE TABLE circle_reports (
    circle_id   BLOB NOT NULL,
    reported_at INTEGER NOT NULL,
    reason      TEXT NOT NULL,
    PRIMARY KEY (circle_id, reported_at)
) STRICT;
```

The unread counter for a sidebar circle row is `SELECT COUNT(*) FROM circle_messages WHERE circle_id = ? AND sent_at > last_read_at`. The hang pulse for a sidebar circle row is `SELECT COUNT(*) >= 3 FROM circle_messages WHERE circle_id = ? AND sent_at > now() - 10 minutes`.

### Sidebar "Circles" section

A new factory under `app/src/sidebar.rs`, mirroring the existing `peers_factory` pattern:

- New `circles_factory: FactoryVecDeque<CircleRow>` in the sidebar's init.
- New header label `"Circles"` placed below the static nav and above `"Others"`. Both headers participate in the same auto-hide logic — show iff the factory is non-empty.
- A new `SidebarMsg::SetCirclesVisible(bool)` and a new `CircleRow` component in `app/src/circle_row.rs`.

`CircleRow` layout: anchor glyph (icon distinguishing AstroConcept / UserChart / FreeForm) · circle title · unread counter (badge on the right) · pulse indicator (small dot near the title, visible iff `hang_active == true`). Click opens the per-circle content-stack page.

### Per-circle content-stack page

`app/src/circle_page.rs`: a new content-stack page registered under a deterministic name (`circle:{hex_circle_id}`), same pattern as `stargazer:{hex_pubkey}` today.

Layout, top to bottom:
- Header: title, anchor row (kind + payload as text — *Phase G replaces this row with the natal-chart wheel for AstroConcept and UserChart anchors*), visibility label ("Public"), member count, "Leave" button.
- Body: `gtk::ListBox` of message rows, scrolled to bottom, virtualised via `FactoryVecDeque<MessageRow>` for circles with long history.
- Footer: `gtk::Entry` + send button. Empty-state placeholder text reflects the anchor ("Say something about Venus square Pluto…").

A new `AppMsg::CircleSendMessage { circle_id, body }` plumbs into a small `zodia-circles` crate's `publish_message(circle_id, body)` helper, which signs and publishes a `CircleOp::Message` op via the existing `ZodiaSyncNode::publish` API (extended to accept the `topic_class` argument).

### `zodia-circles` crate (introduced here, expanded later)

A new crate, scaffolded in Phase F but kept thin: just the helpers around creating, joining, leaving, sending. The PRD's planned location for circle encryption (group keys, rotation, revocation) is this crate; Phase J fills that in. Public-only Phase F needs no encryption code.

Public surface in Phase F:
```rust
pub fn create_public_circle(anchor: Anchor, title: String) -> Result<Hash>;
pub fn join_circle(circle_id: Hash) -> Result<()>;
pub fn leave_circle(circle_id: Hash) -> Result<()>;
pub fn close_circle(circle_id: Hash) -> Result<()>;   // creator-only
pub fn publish_message(circle_id: Hash, body: String) -> Result<()>;
```

### Sky surfacing (extends Phase E's `FeedView`)

Two new `FeedItem` variants — added under the same renderer Phase E shipped:

```rust
enum FeedItem {
    // ... existing Phase E variants ...
    CircleAnchoredOnYou {
        op_id:    Hash,
        creator:  VerifyingKey,
        circle_id: Hash,
        title:     String,
        targets_me: bool,   // always true for this variant
    },
    CircleOnYourKey {
        op_id:    Hash,
        creator:  VerifyingKey,
        circle_id: Hash,
        interp_key: String,
        title:    String,
        targets_me: bool,   // false (does not badge the bell)
    },
}
```

Routing rule from `StateEvent::CircleAnnounced`:
- If `anchor == UserChart(my_pubkey)` → emit `CircleAnchoredOnYou` (targets-me, badges bell, loud Sky card with "Join" CTA).
- Else if `anchor == AstroConcept(k)` and `k ∈ my natal chart keys` → emit `CircleOnYourKey` (ambient Sky card, no bell badge, "Join" CTA).
- Else → no Sky surfacing (still browsable in Network tab).
- `FreeForm` anchors never surface in Sky; they live in the Network tab only.

Per-aspect page surfacing: the per-aspect `FeedView` (Phase E, filtered by `interp_key`) also shows `CircleOnYourKey` cards for that key, regardless of whether the key is in the user's chart — so visiting "Venus square Pluto" shows active circles on that aspect even if it isn't in your natal chart.

### Network tab additions

Today the Network tab shows discoverable peers + recent community contributions + sync activity. Phase F adds a third section: **"Circles on the network"** — a `FactoryVecDeque` of `CircleBrowseRow` widgets, populated from the local mirror of the global circle-index topic.

Browse row layout: title · anchor (kind + payload) · creator pubkey-tag · member count · "Join" button · "Report" overflow action.

Filter chips at the top of the section: All / AstroConcept / UserChart / FreeForm. (Stretch goal: search by anchor payload — defer if it slows the milestone.)

`Report` writes a row to the local `circle_reports` table only. Federated moderation (cross-peer blocklists, op-level filtering, reputation) is explicitly out of scope; the row is the audit substrate later phases will build on.

### Hang detection

A SQL view computed on demand:

```sql
CREATE VIEW circles_active_now AS
SELECT circle_id
FROM circle_messages
WHERE sent_at > unixepoch() - 600
GROUP BY circle_id
HAVING COUNT(*) >= 3;
```

Sidebar rows and Sky cards consult this view; the sidebar refreshes the pulse indicator on a 30-second tick or whenever a new message arrives. Phase H replaces this view with a real-time presence query against the capability-negotiated presence sub-stream; the `circles_active_now` *name* and *result shape* stay the same so consumers don't need to change.

### What changes in existing code

- `zodia-ops`: new `CircleOp` and `CircleAnnounceOp` enums + tests.
- `zodia-pipeline`: `StateEvent` gets four new variants; `DecodeProcessor` becomes topic-class-aware; `MaterializationProcessor` gets a `circle::materialise` arm.
- `zodia-store`: new tables (above); new `insert_*` helpers; new queries (`circles_active_now`, unread counter).
- `zodia-sync` / `zodia-net`: `publish` extended to accept the topic-class argument; the LogSync wire-up tags incoming ops with their topic class.
- `app/`:
  - `sidebar.rs`: Circles section + factory.
  - New `circle_row.rs`, `circle_page.rs`, `circle_create_dialog.rs` (the entry-point modal for creating a circle).
  - `feed_view.rs` (from Phase E): two new card variants.
  - `network_tab.rs`: new "Circles on the network" section + filter chips.
  - `app.rs`: new `AppMsg` variants — `OpenCircle`, `CreateCircle`, `JoinCircle`, `LeaveCircle`, `SendCircleMessage`, `ReportCircle`.

## Testing Decisions

Phase F is the first phase that adds new ops to the wire format since the original PRD's design, so the test surface is broader than Phase E's:

- **`zodia-ops` codec.** Round-trip ser/de tests for every `CircleOp` and `CircleAnnounceOp` variant. Forward-compat test: a decoder cleanly ignores extra map keys inside a known variant. Anchor variants exercised independently. `FreeForm` length cap enforced.
- **`zodia-pipeline` materialisation of circle ops.** Feed a fake `Stream<Operation<()>>` tagged with topic class; assert the expected `StateEvent` shape for each variant. Cover the topic-class-mismatch case (`CircleOp` arriving on the index topic, `CircleAnnounceOp` on a per-circle topic) — both must drop with a `Skipped { reason: TopicClassMismatch }`.
- **`zodia-circles` create/join/leave/publish.** Against an in-memory `ZodiaSyncNode` mock: create a circle, assert `CircleAnnounceOp::Created` published on index topic and `CircleOp::Join` published on per-circle topic. Leave triggers `CircleOp::Leave`. Close (creator-only) triggers `CircleAnnounceOp::Closed`; non-creator close attempt errors locally.
- **Sky routing.** Unit test the `StateEvent::CircleAnnounced` → `FeedItem` routing for each anchor type against a fixture chart; assert the bell-targeting flag is correct.
- **Hang view.** Insert N messages at fake timestamps; assert `circles_active_now` returns exactly the set of circles with ≥3 messages in the last 10 minutes.
- **Sidebar visibility lifecycle.** Component test: empty factory → header hidden; insert a row → header visible; remove last row → header hidden again. Mirrors the existing "Others" header logic.
- **Unread counter correctness.** Simulate a sequence of incoming messages with the user reading partway through; assert the counter reflects the gap and resets to zero when the circle page is opened.

We continue to skip live-iroh integration tests at the Zodia layer; the topic substrate is exercised at the `p2panda-net` level upstream.

## Out of Scope

- **Audio in circles.** Multi-client live audio (mesh ≤6 per the design conversation) is Phase I. Phase F circles are text-only.
- **Recording and replay.** CAS-stored per-participant audio segments with attribution are Phase I.
- **Private circles and group encryption.** Per the grilling: the privacy option is *only* on UserChart anchors, and shipping it requires the PRD's planned group-key infrastructure (`zodia-circles` encryption layer, key rotation, revocation). All Phase J. Phase F ships public-only.
- **Chart key export/import.** The identity-loss mitigation surfaced during the privacy discussion lands with Phase J because it pairs with private-circle membership identity continuity.
- **Chart wheel visualization.** The anchor row in the circle page shows text in Phase F. Phase G replaces it with the Cairo-rendered natal wheel.
- **Real-time presence.** Phase F's hang signal is *derived* (recent message count). Phase H replaces it with capability-negotiated presence ops.
- **Federated moderation.** `Report` writes a local row; cross-peer enforcement (mute lists, op-level filtering, reputation, banning) is its own design discussion later.
- **Threaded messages within circles.** Phase F message ops are flat. Threading inside a circle (à la `InterpOp::RespondTo` for circles) is a future feature.
- **Editing or deleting circle messages.** Send-and-stay. Edit/delete is a separate design conversation.
- **Per-anchor circle-index topics.** Phase F uses a single global index. Sharding into per-anchor index topics is a scaling concern for a later phase, only if growth requires it.
- **Lazy per-key topic subscription** (original C-2). Not a prerequisite for Phase F. Can interleave whenever convenient.
- **Pruning of circle ops.** Storage retention is Phase K.

## Further Notes

**Version cut.** Phase F adds new op variants to the wire format. Peers on 0.7.x or 0.8.x (Phase E) will receive `CircleOp` / `CircleAnnounceOp` ops and drop them at decode (Skipped::MalformedOp) — their pipeline does not yet understand circles. They are not broken; they simply don't see circles. Phase F therefore ships as **0.9.0** with a release note that circles between 0.9+ and < 0.9 peers won't appear on the older side until they upgrade.

**Why this phase before audio.** The grilling produced a clear sequence: cash in B/C-1 (Phase E feed) → ship the killer feature substrate text-only (Phase F) → make the visual identity (Phase G charts) → make it real-time (Phase H presence) → add voice (Phase I audio). The temptation to fold audio into Phase F is large — a circle "feels" complete with voice — but the cost is bundling four engineering risks (mesh transport, recording, blob CAS, replay UI) into one ship. Phase F as text-only lets the social affordances and the wire format land first; voice slots onto a known-good substrate.

**Why public-only.** The privacy axis is *only* on UserChart anchors per the grilling outcome. Phase F skips it because the privacy mechanism (PRD's group encryption, key rotation, the subject-is-auto-member invariant) is the most complex piece of the original D-circles work. Shipping public-only circles first means we learn what the social affordances feel like *before* committing to the encrypted-circle implementation choices.

**Connection to the original PRD.** The operations-and-streams PRD's Phase D bundled "stream-negotiated pair channels + group encryption + pruning" into one phase. The grilling decomposed Phase D into separate phases (H pair-channel rework, I audio, J private circles, K pruning). Phase F is *new* — not in the original PRD — and sits on top of the foundation Phases A–C-1 established.

**Open questions to resolve during implementation.**

- Anchor display formatting: how `UserChart(pubkey)` shows when the local node doesn't know the peer's chosen nickname yet — fall back to a 4-hex-byte tag, presumably, then upgrade when the announce blob lands.
- The exact pulse animation for "hang active now" — a designer/UX call, not a wire-format question.
- Whether "circle created about me while I was offline" should also fire a system notification (libnotify), in addition to the Sky card and bell badge. Lean yes, given that's the most you-targeting event possible in the app, but verify with manual testing.
- Whether `CircleOp::Join` is required-on-creation or implicit-on-first-message. Lean implicit, so creators don't end up with a duplicate self-join op.
- Whether the global circle-index topic carries past CircleAnnounceOps via LogSync replay on first connect, or only new ones. Lean: LogSync replays back to a configurable horizon (default 30 days) so newly-joining nodes don't have an empty Network tab. Decide in PR.

**Risks.**

- **Global circle-index topic scaling.** At 10k active users creating circles, the topic receives 10s–100s of small ops per hour. Fine. At 1M, this becomes a bottleneck — per-anchor index topics or a Bloom-filter-based gossip filter is the future answer. Validate at MVP volumes, plan the shard for later.
- **The single-author content-hash circle_id.** `BLAKE3(creator_pk || anchor_cbor || created_at)` makes circle_ids collision-resistant but the same user creating two circles with the same anchor in the same millisecond produces the same id — guard with a tiny random nonce field if testing surfaces this.
- **Sky card noise on launch.** A user opening Phase F for the first time after a long absence may face a wall of `CircleOnYourKey` cards for every circle ever formed on any aspect in their chart. Mitigate: cap to most-recent N at first paint; add the "older events" summary card pattern from Phase E.
- **`zodia-circles` as a crate.** Adding a new crate has a small ergonomic cost; resist the temptation to put circle code directly in `zodia-sync`. The Phase J encryption work needs a separation of concerns.

**Naming.** "Circle" in user-facing copy throughout. "Hang" is internal jargon — appears in code names (`circles_active_now`, `hang_active`) but does not appear in any UI string in Phase F. The user-facing pulse indicator carries no text; it just pulses.
