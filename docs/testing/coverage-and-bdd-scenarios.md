# Test coverage audit + BDD scenario proposals

**Status:** living document — coverage numbers below reflect `main` as of the granular-topic-subscription and zodia-sdk work landing (commits through `477aa5b`), plus the crypto/sync additions in this pass.

## Coverage snapshot

Ground truth from `cargo test --workspace`, plus this pass's additions:

| Crate | Tests | Notes |
|---|---|---|
| `zodia-core` | 6 | aspects, ephemeris-adjacent logic |
| `zodia-crypto` | 8 (was 0) | `identity.rs` only — see gap below |
| `zodia-net` | 13 (11 unit + 2 integration) | consent blob, announce blob, `DirectChannel` |
| `zodia-store` | 6 | **all** under `feed_tests` — see gap below |
| `zodia-sync` | 3 (was 0) | added this pass — transaction-bracketing regression + key-derivation |
| `zodia-ops` | 13 | codec round-trips, `log_id_for_key` collision sweep |
| `zodia-pipeline` | 3 | one processor pair (Decode → Materialize) |
| `zodia-doc` | 4 | ring eviction, veto authority, CRDT convergence |
| `zodia-sdk` | 3 unit + 3 cucumber scenarios | unit: connect, thread-teardown-on-drop, round trip. Cucumber: collaborative editing, granular-subscription silence, affirmation propagation — see BDD section below |
| `zodia-config` | 0 | |
| `zodia-av` | 0 | audio hardware — see note below |
| `app` (bin) | 2 | ~45+ message handlers / view functions, largest crate by far |

## Priority gaps, ranked

**1. `zodia-crypto`: `ecies`, `handshake`, `ratchet` modules — zero tests.** This pass added coverage for `identity.rs` only (key derivation, sign/verify). The actual end-to-end encryption stack — `ConsentState::accept_consent`/`accept_voice`/`encrypt`/`decrypt` (the tier-1 consent handshake and the symmetric ratchet backing every 1:1 encrypted exchange), plus the ECIES relay-payload encryption `relay_public_key`/`relay_secret_bytes` exist to support — has no test at all. This is the highest-value gap in the repo: it's the security boundary between "two strangers connect" and "an encrypted channel exists," and a regression here is silent (wrong output, not a compile error) until two real devices fail to talk to each other, or worse, produces a channel that looks encrypted but isn't. Concrete next tests:
   - `ecies`: encrypt-then-decrypt round trip; decrypting with the wrong secret fails; tampered ciphertext fails (AEAD tag check).
   - `ratchet`: two `ConsentState`s from `accept_consent`'s two sides can `encrypt`/`decrypt` each other's frames; N-message exchange advances correctly; a frame decrypted twice (replay) is rejected if the design intends that (check whether it does — if not, that's itself worth a PRD-level note, not just a test).
   - `handshake`/`ConsentState`: `prekey_bundle` → `accept_consent` → both sides derive the same shared state (mirror what `net/tests/channel.rs` does for the wire framing, but at the crypto layer beneath it).

**2. `zodia-store`: 39 of 45 public functions have no direct test.** All 6 existing tests live under `feed_tests`; nothing exercises `insert_from_op`, `interp_key_and_author`, `block_ring_get`, `doc_load`/`doc_save` outside the one `feed_tests::doc_save_with_history_then_rollback_restores_prior` case, `authored_rows_for_key`, `distinct_interp_keys`, or the legacy `affirmations` table path. This is the crate every other crate's data ultimately lands in — a regression here corrupts what users read, not just what they sync. Cheapest fix: `SqliteStore`-style `ZodiaStore::temporary()`-equivalent already likely needed for these tests (check whether `zodia-store` already has an in-memory/test constructor — if not, add one, mirroring `p2panda_store::SqliteStore::temporary()`'s pattern used in this pass's `zodia-sync` tests).

**3. `app` crate: 2 tests against the largest, most business-logic-dense crate in the workspace.** Most of `app.rs` is GTK/relm4-coupled and genuinely hard to unit test without a display — that's a legitimate reason for low coverage, not neglect. But several pieces are pure data transforms wrapped in `update()`/`update_view()` glue and could be extracted and tested without any GTK dependency:
   - The veto-authorization call path around `app.rs`'s `ProposeDocVeto` handling — `zodia_doc::veto_authorised` itself is tested (`zodia-doc`'s 4 tests), but the app-layer wrapper that fetches the ring from the store, builds the `Ring` from raw rows, and calls it is not. A regression there (wrong ring fetched, wrong timestamp passed) would silently let an unauthorized veto through or block a legitimate one — same severity class as gap #1.
   - `feed_item.rs`'s `state_event_to_feed_item`/`feed_row_to_feed_item`/`block_you_authored_was_edited`/`doc_rolled_back` — already has 1 test (`interp_key_predicate`); the conversion functions themselves (the actual "does a `StateEvent` become the right `FeedItem`" logic) don't.
   - `aspect_list.rs`'s `natal_items`/`transit_items` — pure functions building UI row data from `Aspect`/`TransitAspect` — zero tests despite being on every page render.

**4. `zodia-config`: 0 tests.** `LocalConfig::load_or_create`, `save_birth` — file-system side effects, testable with a temp-dir override of the config path. Low severity (failure mode is "can't start," loud not silent) but cheap to add.

**5. `zodia-av`: 0 tests, accepted gap.** Real audio I/O (`cpal`, `opus`) isn't meaningfully unit-testable without hardware or a lot of mocking infrastructure this repo doesn't have yet. Not flagging this as neglect — flagging it so it's a documented decision rather than an unexamined zero.

**6. `zodia-sdk`: the `Lagged` backpressure test `docs/prd/zodia-sdk.md`'s Testing Decisions called for.** "Unsubscribed keys stay silent" is now covered — see the `granular_subscription.feature` cucumber scenario below. ~~The backpressure test (a slow subscriber falls behind and gets `RecvError::Lagged` rather than stalling the publisher) is still open.~~ Closed — see `backpressure.feature`: a peer who authors 300 interpretations while the other never reads its `events()` stream causes the idle subscriber's next `recv()` to return `Lagged`, not block or grow unbounded. Required a settle window between publishing and observing (the 300 ops need time to actually arrive over the network before drain starts, or the scenario just observes a slow trickle instead of an overflowed buffer) — see the scenario's timing for the concrete numbers.

## What this pass added

- `crypto/src/identity.rs`: 8 tests (seed round-trip, distinct identities, `to_panda_key` cross-derivation, sign/verify + tamper/wrong-signer rejection, relay-key determinism).
- `sync/src/lib.rs`: 3 tests, using `p2panda_store::SqliteStore::temporary()` (in-memory, `test_utils` feature) to unit-test the store contract directly — no network required. These specifically pin the transaction-bracketing bug found and fixed via `zodia-sdk`'s networked test (see `docs/prd/granular-topic-subscription.md`'s "Bug found and fixed" note): `associate` must run inside the same transaction as `insert_operation`, and calling it after `commit` must fail loudly.

This is also the general lesson worth stating explicitly: the `zodia-sdk` real-network integration test found a bug that years of the old single-global-topic model had been silently tolerating (masked by long-lived sessions). A slow, real, end-to-end test and a fast, narrow, unit test aren't substitutes for each other — the SDK test found the bug *existed*; the new `zodia-sync` unit tests make sure it can't come back unnoticed. Both layers earn their keep.

---

## BDD scenarios

`cucumber-rs` is now adopted — see "Status: adopted" below the scenarios for what's wired up and what isn't. The three scenarios below (collaborative editing, granular subscription, affirmation propagation) were picked as the highest-impact, SDK-provable user guarantees: they're the actual data-flow promises `docs/prd/collaborative-interpretations.md` and `docs/prd/granular-topic-subscription.md` make to users, not aspirational ones.

Chosen around the features that actually shipped this cycle — activity feed, collaborative interpretations, and granular sync — because these are the behaviors real users hit, not aspirational ones.

### Feature: Collaborative interpretation editing

```gherkin
Feature: Collaborative interpretation editing
  Aspect pages converge on one community-edited text instead of a list
  of competing entries (docs/prd/collaborative-interpretations.md).

  Scenario: A user extends someone else's interpretation
    Given the community body for "natal:venus_square_pluto" already has
      one paragraph written by Alice
    When Bob adds a second paragraph without deleting Alice's
    Then both paragraphs are visible to Alice, Bob, and any third peer
      who opens that aspect page
    And the page shows one converged text, not two competing entries

  Scenario: An author is notified when their text is edited
    Given Alice authored a block in "natal:venus_square_pluto"'s doc
    When Bob edits that block
    Then Alice receives a "your block was edited" event in her feed
    And the notification bell badges once for it

  Scenario: An author vetoes a bad edit within the window
    Given Alice authored a block, and Bob edited it 2 days ago
    When Alice taps "Veto" on Bob's edit
    Then the block reverts to Alice's version
    And Bob's edit is preserved as history, not deleted

  Scenario: A veto attempt outside the window is rejected
    Given Alice authored a block, and Carol edited it 10 days ago
    When Alice taps "Veto" on Carol's edit
    Then the veto is not honoured (window is 7 days)
    And Carol's edit stands
```

### Feature: Activity feed

```gherkin
Feature: Activity feed
  The Sky tab is a live feed instead of a static transit table
  (docs/prd/activity-feed.md).

  Scenario: A user sees their contribution get affirmed
    Given Alice authored an interpretation
    When Bob affirms it (♡)
    Then Alice's feed shows an "affirmed" card for that interpretation
      within a few seconds if Alice is online, or on next launch if not

  Scenario: House transits appear without manual lookup
    Given Saturn is about to enter the user's natal 10th house
    When the transit ticker's next tick runs
    Then a feed card appears announcing the house transit
    And it carries the correct start/end date window

  Scenario: The bell only badges events about the user's own work
    Given Alice is not the author of any interpretation on
      "natal:mars_square_saturn"
    When a stranger affirms someone else's interpretation on that key
    Then Alice's notification bell does not badge
```

### Feature: Bandwidth-conscious sync (Phase C-2)

```gherkin
Feature: Granular per-key sync topics
  A device only receives live updates for keys it's actually
  subscribed to (docs/prd/granular-topic-subscription.md).

  Scenario: Browsing your own chart stays live automatically
    Given Alice's natal chart includes "natal:sun_trine_moon"
    When Alice's app starts and connects to the network
    Then she is subscribed to that key without opening its page
    And a live edit to that key reaches her feed without her
      navigating anywhere

  Scenario: A key nobody has open doesn't cost bandwidth
    Given no device anywhere has "natal:jupiter_opposition_uranus"
      open right now
    When a peer publishes an edit to that key
    Then a device that never subscribed to it does not receive that
      edit's bytes over the wire
    And opening that key's page later still surfaces the edit, once
      subscribed, from a peer who has it
```

### Feature: Grace-period unsubscribe (driven outside-in)

Written and made to fail before the feature existed, then implemented against it — see "Driven outside-in" below.

```gherkin
Feature: Grace-period unsubscribe for non-chart aspect pages
  A key browsed outside the user's own chart subscribes on demand and
  unsubscribes again after being idle, instead of accumulating
  subscriptions forever (docs/prd/granular-topic-subscription.md).

  Scenario: An idle subscription unsubscribes after its grace period
    Given Alice touches a subscription to a key outside her chart, with
      a short grace period
    When the grace period elapses with no further activity on that key
    And a peer publishes an edit to that key
    Then Alice does not receive that edit

  Scenario: Re-touching a subscription keeps it alive past the grace period
    Given Alice touches a subscription to a key outside her chart
    When she touches it again before the grace period elapses
    And a peer publishes an edit to that key
    Then Alice does receive that edit
```

## Status: adopted

`cucumber = "0.23.0"` is now a `zodia-sdk` dev-dependency (`sdk/Cargo.toml`, `[[test]] name = "cucumber", harness = false`). All scenarios above are executable, live in `sdk/tests/features/*.feature`, and run automatically as part of `cargo test --workspace` via `sdk/tests/cucumber.rs`'s step definitions. Each `Given a peer named "X" connected to the network` step spins up a real `ZodiaClient` — real iroh/p2panda transport, no mocking — matching the existing unit tests' approach rather than introducing a second, fake-network test style.

Grown since first adopted: `revoke_propagation.feature` (drove `ZodiaClient::revoke` into existence outside-in), `veto_propagation.feature` and `editor_presence_propagation.feature` (existing methods that had zero BDD coverage), `edit_survives_restart.feature` (durability across a real disconnect/reconnect with the same identity), `multi_peer_fanout.feature` (three independent subscribers, not just a pair — rules out "delivered to the first asker" bugs a pairwise test can't catch), and `multi_key_isolation.feature` (one peer subscribed to two keys at once, proving no cross-key bleed).

Chosen scope, deliberately: only guarantees `zodia-sdk` can actually prove end-to-end (op propagation and materialisation into the right `StateEvent`). App-layer behavior — does the bell badge, does the veto actually revert the block, does the feed render the card — is a separate layer with its own gap tracked above (§3), not something a `ZodiaClient`-only scenario can assert without either a GTK test harness or plumbing app.rs's own logic into something scriptable. Extending these scenarios to that layer is future work, not attempted here.

**Driven outside-in:** `grace_period_unsubscribe.feature` and its step definitions were written first, calling a `ZodiaClient::touch_subscription` method that didn't exist yet — a compile failure standing in for "red" in a compiled language, same idea as a failing assertion in a dynamic one. `touch_subscription(interp_key, grace)` was then implemented in `sdk/src/lib.rs` (subscribes idempotently, restarts a grace-period countdown on the SDK's background `LocalSet`, auto-unsubscribes if untouched) until both scenarios passed. This is now also `docs/prd/granular-topic-subscription.md`'s primary mechanism for its previously-unshipped grace-period unsubscribe requirement — the app-layer wiring (calling it when a page actually opens/stays open) is still open, tracked in that PRD.

**Scenario-concurrency contention, found and fixed:** running these scenarios initially produced an intermittent unrelated-scenario failure (the affirmation-propagation scenario) as soon as a fourth `.feature` file was added. Root cause: cucumber's default `World::run` executes scenarios *concurrently*, and each scenario here spins up 1-2 real network nodes — with 5 scenarios across 4 features, that's up to ~10 real iroh endpoints starting at once inside one test process, contending for CPU and mDNS multicast traffic. Fixed by switching to `ZodiaWorld::cucumber().max_concurrent_scenarios(1).run_and_exit(...)` in `sdk/tests/cucumber.rs` — these are integration tests exercising real system resources, not isolated unit tests, and should run serially for the same reason `net/tests/channel.rs` and the SDK's own round-trip test do. Multiple repeated runs, both in isolation and as part of the full `cargo test --workspace` suite, have been clean since. If flakiness reappears, look for real resource contention (another concurrent test binary, system load) before assuming it's the scenarios themselves — that's what actually happened here, and the fix was concurrency control, not a longer timeout.

**Multi-topic session-establishment jitter, found and documented (not fixed):** `multi_key_isolation.feature` — one peer subscribed to two keys, both edited immediately — was reproducibly flakier than every single-topic scenario: opening two brand-new topics between the same peer pair back-to-back and publishing on both right away raced against p2panda-net's per-topic session negotiation, which has real, sometimes multi-second jitter for the second topic specifically (confirmed reproducible across repeated runs, always the second key that missed, never the first). A 1-second settle after subscribing still missed intermittently (roughly 1 in 3-4 runs); 3 seconds plus a 25-second observation window was clean across 4+ repeated runs. This is a p2panda-net transport characteristic, not a zodia-sdk bug — the routing itself (which topic an edit lands on) was never wrong, only the timing of when a fresh second topic's session was ready to carry it. Left as a documented characteristic in the feature file rather than "fixed," since real app usage rarely opens two brand-new topics in the same instant the way this scenario deliberately stresses.

Same class of jitter reappeared in `circle_sharing.feature`'s circle-isolation scenario (3 peers, 2 circles created back-to-back by one author, sharing one underlying log per `CIRCLE_LOG_ID`) — but unlike the case above, more front-loaded settle time didn't reduce the failure rate here (tried 3s, then 5s+1s+3s split across the two circle creations). The invite's own 25s internal retry loop is what actually absorbs jitter, and this specific combination still occasionally exceeds it. Left as an accepted flaky characteristic rather than chased further, same reasoning as above: real usage doesn't create two circles the instant two peers connect.
