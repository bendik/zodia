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
| `zodia-sdk` | 3 | connect, thread-teardown-on-drop, real 2-client networked round trip |
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

**6. `zodia-sdk`: the two tests `docs/prd/zodia-sdk.md`'s Testing Decisions called for and didn't get.** "Unsubscribed keys stay silent" (the direct regression test for Phase C-2's whole premise) and the `Lagged` backpressure test are still open — noted in that PRD, repeating here because they're cheap now that the round-trip test's plumbing already exists to extend.

## What this pass added

- `crypto/src/identity.rs`: 8 tests (seed round-trip, distinct identities, `to_panda_key` cross-derivation, sign/verify + tamper/wrong-signer rejection, relay-key determinism).
- `sync/src/lib.rs`: 3 tests, using `p2panda_store::SqliteStore::temporary()` (in-memory, `test_utils` feature) to unit-test the store contract directly — no network required. These specifically pin the transaction-bracketing bug found and fixed via `zodia-sdk`'s networked test (see `docs/prd/granular-topic-subscription.md`'s "Bug found and fixed" note): `associate` must run inside the same transaction as `insert_operation`, and calling it after `commit` must fail loudly.

This is also the general lesson worth stating explicitly: the `zodia-sdk` real-network integration test found a bug that years of the old single-global-topic model had been silently tolerating (masked by long-lived sessions). A slow, real, end-to-end test and a fast, narrow, unit test aren't substitutes for each other — the SDK test found the bug *existed*; the new `zodia-sync` unit tests make sure it can't come back unnoticed. Both layers earn their keep.

---

## BDD scenarios

No BDD test-runner (cucumber-rs or similar) is set up in this workspace — these are written as Gherkin-style scenarios documenting user-facing behavior, not wired to an executable runner. Adopting one is a separate decision (new dependency, new CI step) worth asking about explicitly rather than assuming; these scenarios are useful as acceptance criteria and as a map of what a runner *would* exercise if adopted, independent of that decision.

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

## Open question worth asking, not deciding here

Should this repo adopt an actual BDD runner (`cucumber-rs` is the natural Rust choice) so the scenarios above become executable against `zodia-sdk`? The SDK's existing test harness (real two-client networked tests, no GTK) is already the right shape to back `.feature` files almost directly — `Given` a subscribed client, `When` a `client.edit(...)` call, `Then` an `events()` assertion. That's a real option now that didn't exist before `zodia-sdk` landed. Flagging it rather than deciding it: adopting a new test framework and `.feature` file convention is a team-workflow decision, not a unilateral one.
