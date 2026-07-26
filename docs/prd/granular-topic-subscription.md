# PRD: Granular per-key sync topics with lazy subscription (Phase C-2)

**Status:** shipped — log-splitting, always-subscribe, the grace-period unsubscribe primitive, and `app.rs` wiring to real page visibility are all done. Only mock-backed `zodia-sync` tests remain open (low priority — this crate's contracts are already covered by real-network tests in `zodia-sdk`, see that PRD and `docs/testing/coverage-and-bdd-scenarios.md`).
**Branch:** `main`
**Foundation already landed:** 0.7.0 (`zodia-ops` + `zodia-pipeline`, single global sync topic) and 0.7.1 (activity feed, collaborative interpretations — both still ride the single global topic).
**Supersedes:** the "Topic granularity and lazy subscription" sketch in `docs/prd/operations-and-streams-rearchitecture.md` (§ Implementation Decisions). That sketch assumed per-key topics were a routing-layer change; this PRD corrects that assumption against what actually shipped and defines the real migration.

## Problem Statement

`docs/prd/operations-and-streams-rearchitecture.md` originally described the target state as "the app subscribes lazily — on entering an aspect detail page, subscribe; on leaving for >N minutes, unsubscribe," with per-`interp_key` topics `Topic::from(blake3("interp:" || interp_key))`. That never shipped. What actually exists as of 0.7.1:

- **Exactly one sync topic, ever.** `ZodiaSyncNode::spawn` (`sync/src/lib.rs:110`) takes a single `sync_topic: Topic` and opens a single `SyncHandle`. The caller always passes `Topic::from(topic_key_global().0)` (`app/src/app.rs:2432-2435`). There is no code path that opens a second sync topic.
- **Exactly one log per author, ever.** `INTERP_LOG_ID: u64 = 0` (`sync/src/lib.rs:73`) is a constant, not a derived value. Every `InterpOp` and every `DocOp` a device ever publishes — natal aspects, sky aspects, house transits, doc edits, vetoes, affirmations, editor presence — lands in the same backlinked chain (`get_latest_entry(&verifying_key, &INTERP_LOG_ID)`, `sync/src/lib.rs:225`).
- **Consequence: full replication to every peer, forever.** Because LogSync's unit of replication is a whole `(author, log_id)` log synced over its topic, and there is one log per author on one topic, every peer that syncs with you receives every op you've ever published for every key — including keys nowhere near their chart, keys they'll never open a page for, and years-old history. There is no way today to receive *some* of a peer's operations without receiving *all* of them.
- **This is a stronger version of the original problem, not a smaller one.** The original Problem Statement worried about Tier-0 gossip discovery topics (`topic_keys_for_chart`, `net/src/network.rs:129-132` — ~20-28 degree-bucket topics, eager at startup, used only for peer *discovery*). That's a separate, already-bounded mechanism and isn't what's at issue here. The real cost is the LogSync *content* replication layer, which has no partitioning at all — it's worse than "eager subscription to your own chart's topics," it's "eager subscription to everyone's everything."
- **Bandwidth and storage both grow with total community activity, not with what a user actually reads.** A device that has only ever opened five aspect pages still downloads and stores every `DocOp::Edit` anyone on the network makes to any key, because there is no other unit of subscription available. This is User Story 6 from the original PRD ("I don't want my device to subscribe to topics I'm not actively browsing") — unaddressed, and getting more expensive every release that adds op traffic (0.7.1 added `DocOp` on top of `InterpOp` onto the same single log).
- **Local storage has no corresponding pruning yet either** (that's Phase D / K), so today the only lever available for bounding growth is *not receiving data in the first place* — which makes this phase higher-leverage than its position in the original phase order suggested.

## Solution

Two changes, evaluated together because the second is what makes the first possible:

1. **Derive `log_id` from `interp_key` instead of hardcoding it to `0`.** `log_id = u64::from_le_bytes(blake3("interp-log:v1:" || interp_key)[..8])` (collision handling: see Implementation Decisions). Each key an author publishes to gets its own log. A log by construction contains only that key's ops, so syncing that one `(author, log_id)` pair over its topic delivers *exactly* that key's content — nothing else. This is the piece the original sketch was missing: "subscribe to a topic" only saves bandwidth if the thing you sync when subscribed is actually scoped to what you asked for.
2. **Layer a topic per key on top, joined/left as the user navigates**, per the original design: subscribe on opening an aspect/doc page, unsubscribe after a grace period once no visible page references that key, always-subscribed for the keys in the user's own natal chart (so the home view and Sky feed stay live without per-page churn). The global topic (`topic_key_global()`) stops carrying interpretation content and reverts to what the Problem Statement's own "Solution" section already scoped it for: peer discovery only.

Existing signed operations cannot be moved to a different log — p2panda operation hashes are computed over `(log_id, seq_num, backlink, ...)`, so re-assigning `log_id` on historical ops would invalidate every signature already accepted by every peer that has them. This is not a data-model preference, it's cryptographic: **all pre-migration history permanently stays in log 0.** The migration is forward-only: new ops after the cutover use the derived `log_id`; log 0 keeps being synced (still over the global topic, or a dedicated legacy topic) for backward reads, indefinitely or until a retention policy is layered on top of it separately.

## User Stories

1. As a Zodia user on a metered or slow connection, I want my device to only fetch operations for aspects I've actually opened or that are in my own chart, so background sync doesn't cost me bandwidth for the entire network's activity.

2. As a Zodia user with a mature install, I want my local storage to grow with what I've actually browsed rather than with total community output, so the app doesn't slowly bloat regardless of how I use it.

3. As a Zodia reader browsing an aspect outside my own chart, I want to still be able to read (and eventually receive live updates for) that key's community interpretation once I open its page, so exploration isn't blocked by the new subscription model — it should feel the same as today, just not-always-on.

4. As a Zodia user who closes an aspect page and doesn't return, I want my subscription to that key to wind down automatically after a while, so idle browsing history doesn't accumulate permanent subscriptions.

5. As a Zodia developer, I want a deterministic, collision-safe mapping from `interp_key` to `log_id` that every peer computes independently, so two peers never need to negotiate which log holds a given key's content.

6. As a Zodia user upgrading from 0.7.x, I want my own historical contributions and everyone else's existing community body to keep reading normally after the upgrade, so this migration is invisible except for the bandwidth improvement.

7. As a Zodia developer debugging sync, I want to see, per key, whether the device is currently subscribed and roughly how fresh that key's data is, so subscription churn bugs are diagnosable rather than silent.

## Implementation Decisions

### `log_id` derivation

```
log_id: u64 = u64::from_le_bytes(blake3(b"interp-log:v1:" || interp_key.as_bytes())[..8])
```

Collisions are theoretically possible (birthday bound at ~2^32 keys per author) but not practically reachable — no author will publish to billions of distinct keys. `zodia-ops` gets a pure function `fn log_id_for_key(interp_key: &str) -> u64` that both the publish path (`sync::publish` / `publish_doc`) and the subscribe path (topic manager) call, so there is exactly one source of truth. Add a debug assertion in tests that two different keys drawn from the real baseline TOML never collide, so a real collision would be caught in CI rather than in the field.

### `ZodiaSyncNode` becomes multi-log, multi-topic

Today `ZodiaSyncNode` owns one `SyncHandle`. It needs to own a map: `HashMap<TopicKey, SyncHandle<...>>`, plus:

- `subscribe(&mut self, interp_key: &str) -> Result<(), SyncError>` — computes the topic, opens `log_sync.stream(topic, true)`, spawns a forwarder task identical to the existing one in `spawn()`, and stores the handle. No-op if already subscribed.
- `unsubscribe(&mut self, interp_key: &str)` — drops the handle (dropping a `SyncHandle` ends the p2panda-net session for that topic; verify this against `p2panda-net`'s actual `Drop` behavior in Phase A-of-this-phase spike work before committing to the API shape — if handles need explicit teardown rather than `Drop`, add that call here).
- `publish` / `publish_doc` — instead of always resolving to `INTERP_LOG_ID`, look up `log_id_for_key(op.interp_key())`. This requires `InterpOp` and `DocOp` to expose their target key uniformly; today `interp_key` lives on individual variants, not as a shared trait method — add one.
- The legacy `log_id = 0` path stays reachable read-only, for replaying pre-migration history. New writes never target it again after the version cutover.

### Subscription lifecycle policy (relm4 layer)

- **Always-subscribed set**: every `interp_key` present in the user's own natal chart (`chart.natal_aspects()`) plus their currently-active transit set — i.e. exactly the keys the Sky feed and home aspect list already render, so this set is "free" to compute (it's the same set `build_widgets`/`update_view` already iterate to build those views).
- **On-demand subscribe**: opening `aspect_view` for a key not already in the always-subscribed set calls `subscribe`. Multiple simultaneously open pages on the same key just no-op past the first.
- **Unsubscribe grace period**: when the last page referencing a not-always-subscribed key closes, start a timer; unsubscribe if no page for that key reopens before it fires. Reuse the app's existing 10-minute background cadence (`transit_ticker.rs`'s `TICK_INTERVAL`) as the default grace window rather than inventing a new constant — consistent with the app's established rhythm and long enough that flipping between two aspect pages doesn't cause resubscribe churn each time.
- **Teardown on app close**: no special handling needed: the whole `ZodiaSyncNode` (and its handle map) drops with the process.

### What "browsing a key outside your chart" looks like post-migration

Before this phase, opening any aspect page works identically regardless of whether it's in your chart, because the global topic already delivered everything. After this phase, opening a key you've never subscribed to triggers `subscribe`, and you'll receive:

- Any op from a peer publishing under the *new* per-key log scheme, from the moment you subscribe onward (live), plus their history for that key if they're online while you're subscribed (LogSync catch-up works per-log same as it does today for the single log).
- Nothing from a peer who hasn't published to that key since upgrading (their relevant history, if any, is buried in their untouched log 0 and isn't addressable by the new topic).

This is a real, user-visible regression during the transition window — a page that used to show community interpretations instantly may show fewer of them right after upgrade, recovering as more of the network's *active* keys get touched again post-migration. This needs a line in the eventual release notes; see Migration story below.

## Progress notes

Shipped: `log_id_for_key` (`ops/src/lib.rs`), `DocOp::interp_key()`, `topic_key_for_interp` (`core/src/topic.rs`), and the `ZodiaSyncNode` multi-topic refactor (`sync/src/lib.rs` — `HashMap<Topic, SyncHandle>`, `subscribe`/`unsubscribe`, `publish_doc` routed through the derived log/topic, legacy `publish` untouched on log 0). `app/src/app.rs` sends `Subscribe` for every natal-chart key right after sync spawns (both cold-start paths).

Shipped since: the grace-period unsubscribe mechanism, as `ZodiaClient::touch_subscription(interp_key, grace)` in `zodia-sdk` (`sdk/src/lib.rs`) rather than in `zodia-sync` directly — touching a key subscribes it (idempotent) and (re)starts a `grace` countdown on the SDK's background `LocalSet`; if nothing touches it again before `grace` elapses, it auto-unsubscribes. Driven outside-in via two cucumber scenarios in `sdk/tests/features/grace_period_unsubscribe.feature` (idle-unsubscribe, and re-touch-cancels-pending-expiry), both passing against real network transport.

**`app.rs` wiring, done.** `build_doc_reading_group` in `aspect_view.rs` — the same site that already announces `EditorPresence` join on page open — now also sends `AppMsg::TouchKeySubscription { interp_key }`. The handler (`app.rs`) checks `needs_lazy_subscription(interp_key, chart)`: a pure function (BDD-driven — written and tested before being wired in) that's `false` for any key already in the user's own chart (permanently subscribed at startup, touching it would be redundant and could wrongly let it expire) and `true` otherwise, in which case it calls `client.touch_subscription(key, 600s)` — 600s reusing `transit_ticker.rs`'s existing `TICK_INTERVAL` rather than inventing a second cadence constant. Re-opening a page re-touches and resets the grace clock; nothing runs on page-close (`connect_hiding`) since the clock already started at open time — a deliberately simpler model than the original sketch's "count from leaving," see Further Notes below for why.

Not shipped: the mock-backed `ZodiaSyncNode` tests from Testing Decisions below — this crate has no mock `LogSync`/`SyncHandle` harness yet, unlike `zodia-pipeline`'s fake-stream tests. That gap is why the `associate()` bug went unnoticed until `zodia-sdk`'s real-network test caught it. Low priority now: this crate's actual contracts are exercised by `zodia-sdk`'s real-network cucumber suite, which is stronger evidence than a mock ever gives.

**Bug found and fixed via `docs/prd/zodia-sdk.md`'s testing:** `TopicStore::associate(topic, author, log_id)` was never called anywhere in `zodia-sync`, for any topic — meaning catch-up for a peer who subscribes to a topic *after* an op already exists on it could never find that op (`topics_v1` stayed empty). Per-key topics made this much more likely to bite in practice than the old always-on global topic did, since short-lived subscriptions have a much smaller window where an already-open live session happens to mask the missing catch-up path. Fixed in `sync/src/lib.rs::publish_bytes`.

## Testing Decisions

Following the existing pattern (`zodia-pipeline` processors tested against a fake `Stream<InterpOp>`, no live-iroh integration tests):

- **`log_id_for_key`**: pure function, exhaustive collision test against every key in the bundled baseline TOML plus a large synthetic sample.
- **`ZodiaSyncNode` subscribe/unsubscribe**: mock `LogSync`/`SyncHandle` backend (same style as the planned `zodia-channels` handshake tests) — assert subscribing opens exactly one handle per topic, re-subscribing is a no-op, unsubscribing tears down cleanly, and publishing after a fresh subscribe resolves the correct `log_id`.
- **Legacy-log read path**: seed a store with log-0 history, confirm it's still queryable/renderable after the cutover with no writes ever targeting it again.
- **Grace-period timer**: fake clock, assert reopening a page before the timer fires cancels the pending unsubscribe, and assert the timer actually fires and unsubscribes when a key stays untouched.
- No property-based causal-ordering tests are needed here (that's Phase C-1's concern, already shipped and already tested) — this phase only changes *which* logs/topics carry ops, not ordering within a log.

## Out of Scope

- **Pruning old operations.** This phase bounds what a device *receives*; it doesn't prune what it already has. Phase D / K.
- **Discovery of "who has interesting content on key X before I subscribe."** Subscribing is a leap of faith that *someone* is listening on that topic; there's no pre-subscribe preview or popularity signal. Ambient discovery is explicitly out of scope per the parent PRD too.
- **Retroactively re-homing log-0 history into per-key logs.** Cryptographically impossible without re-signing (see Solution) — not attempted.
- **Circle / pair-channel topics.** Those are Phase D's `zodia-channels` / `zodia-circles` concern and use a different topic derivation entirely; this phase only touches `interp:*` topics.
- **Changing how Tier-0 discovery topics work** (`topic_keys_for_chart`). Untouched — that mechanism is already bounded and isn't the problem this phase solves.

## Further Notes

**Why this corrects rather than just implements the original sketch.** The original PRD wrote "per-interp-key topics" as if topic-joining were the whole mechanism. It isn't: p2panda's LogSync replicates whole logs, and Zodia shipped with exactly one log per author (`INTERP_LOG_ID = 0`, a literal constant) rather than the per-key logs the topic-partitioning idea silently assumed. Implementing "subscribe/unsubscribe per topic" on top of the single-log-per-author reality as it actually shipped would do nothing — every peer you meet on any topic would still hand you their one all-keys log in full. The log-splitting change is the load-bearing part; the topic lifecycle policy is the part that was already correctly scoped.

**Version cut.** This changes what `log_id` new operations are published under — a peer still deriving `log_id = 0` for everything won't discover a post-migration peer's new per-key logs (they're on a different topic than the one they're listening to) and vice versa new-model peers don't listen on the old global topic for content by default. Needs the same treatment as the Phase F-collab cut ([[operations-and-streams-rearchitecture]] Phase D precedent, and the correction recorded in `docs/prd/collaborative-interpretations.md`'s two version-cut notes): bump minor, not patch, and say so explicitly in the release description this time — the last phase's release note didn't, and that was a documented mistake, not a decision to repeat.

**Migration story.** Ship as additive: a device starts subscribing to per-key topics and writing to derived logs on upgrade; log 0 keeps being read (still worth syncing it under the global topic, unmodified, so this doesn't strand existing history). No local data rewrite. The transitional "browsing an untouched key shows less than before, temporarily" cost (see Implementation Decisions) is real and should be called out plainly in the release notes rather than discovered by users as a regression report.

**Why grace-from-open, not grace-from-leaving.** The original sketch above described "subscribe on entering a page, unsubscribe after leaving for >N minutes" — a two-phase model where the countdown starts when the page closes. What shipped in `ZodiaClient::touch_subscription` starts the countdown the moment a key is touched (including the *first* touch, on open), and re-opening the page resets it. That means a single continuous page visit longer than the grace period (600s) would let the subscription lapse while still on the page — a real behavioral difference from the original sketch, accepted deliberately: implementing "count from leaving, cancel on return" cleanly would need the SDK to distinguish "still on the page" from "touched once," which `touch_subscription`'s current single-call API doesn't carry. Given typical aspect-page visits (read, maybe edit, navigate on) are well under 10 minutes, this is judged good-enough; a periodic re-touch while a page stays open is the fix if long single sessions turn out to matter in practice.

**Resolved open questions.**

- `p2panda_net::sync::SyncHandle` *does* release its topic subscription on `Drop` (`Drop` impl sends `ToSyncManager::Close(topic)`, confirmed by reading `p2panda-net-0.6.1` source directly) — no explicit async teardown call needed. `ZodiaSyncNode::unsubscribe` just drops the map entry.
- The unsubscribe grace period ended up per-key by construction (`touch_subscription` spawns one timer per touched key) — no global sweep was needed; per-key timers proved simple enough in practice.

**Still open.** Whether there's a meaningful per-topic/per-log resource cost in `p2panda-net`'s gossip/discovery layer at real scale (dozens of always-subscribed own-chart keys plus however many lazily-touched keys a session accumulates) hasn't been measured — worth revisiting with real numbers if it ever becomes a support complaint, not guessed at now.
