# PRD: `zodia-sdk` — relm4-agnostic client facade over the p2p data flow

**Status:** shipped and migrated — `app.rs` runs entirely on `ZodiaClient` now, `SyncPublishMsg`/`ZodiaSyncNode`/`ZodiaPipeline` no longer referenced anywhere in `app/`
**Branch:** `main`
**Foundation already landed:** `zodia-net` (transport/discovery), `zodia-sync` (per-key LogSync, Phase C-2), `zodia-pipeline` (op decode + materialisation → `StateEvent`). All three exist and work; nothing here changes their internals or the wire format.

## Problem Statement

Today "the SDK" is `app/src/app.rs`. Concretely:

- `try_spawn_network` and `try_spawn_sync` (`app/src/app.rs:2395-2480`) own `ZodiaNetwork` and `ZodiaSyncNode` directly, construct a `ZodiaPipeline`, and drive it from a `glib::MainContext::default().spawn_local` task — because `p2panda-stream` is `!Send` (`pipeline/src/lib.rs:18-22`, its own doc comment says so), the pipeline literally cannot leave the relm4 main loop's thread today.
- The publish/subscribe surface (`SyncPublishMsg::{Publish, PublishDoc, Subscribe, Unsubscribe}`) is a `pub(crate)` enum inside the app binary, not a library API. Nothing outside `app/` can open a Zodia connection, subscribe to a key, or read the event stream without re-deriving all of this.
- Outbound `StateEvent`s are dispatched straight into `AppMsg::SyncStateEvent` (relm4's message type) — there's no neutral event stream a non-relm4 consumer could read.
- There is already a second, half-finished consumer in this repo: the `mobile` branch (`initial rewrite`). It has no shared foundation to build on except copy-pasting the glib-coupled plumbing out of `app.rs`.
- Positioning-wise: if Zodia's own desktop app is meant to demonstrate what building *on* the Zodia network looks like, the thing a third party would actually copy today is "own a `glib::MainContext`, understand p2panda's `!Send` constraint, hand-roll a command enum." That's an implementation detail leaking as the API.

## Solution

A new `zodia-sdk` crate: `ZodiaClient`, a single facade type that owns network + sync + pipeline internally and exposes a domain-shaped, runtime-agnostic API — `subscribe`, `edit`, `veto`, `affirm`, `events()`. No topics, logs, `!Send`, or glib anywhere in its public surface.

The core move that makes this possible: **the `!Send` pipeline moves onto its own dedicated OS thread, inside the SDK, running a `tokio::task::LocalSet`.** Everything the SDK exposes outward crosses that thread boundary through `tokio::sync::mpsc` / `broadcast` channels, which are `Send` regardless of what's not-`Send` on the other side. Today that thread happens to be donated by relm4's glib main loop; after this change it's the SDK's own thread, and relm4 (or a CLI, or a test harness, or the `mobile` branch's toolkit) becomes just another consumer reading a channel.

```
┌─────────────────────────── zodia-sdk's dedicated thread ───────────────────────────┐
│  tokio::runtime::Builder::new_current_thread() + LocalSet                          │
│                                                                                      │
│   ZodiaNetwork ──▶ ZodiaSyncNode ──▶ ZodiaPipeline ──▶ StateEvent                   │
│        ▲                  ▲                                    │                    │
│        │            Command::{Subscribe,Unsubscribe,           │                    │
│        │                     PublishEdit,PublishVeto,...}       │                    │
└────────┼──────────────────┼──────────────────────────────────── ┼────────────────────┘
         │            mpsc::Sender<Command>            broadcast::Sender<StateEvent>
         │                  │                                     │
         └──────────────────┴──────────────  ZodiaClient (Send, 'static) ─────────────┐
                                                       │                                │
                                            any async runtime: relm4/glib bridge,       │
                                            tokio multi-thread, plain std::thread, ...   │
```

## User Stories

1. As the Zodia desktop app, I want to open a connection and subscribe to keys through a small set of domain verbs, so `app.rs` stops owning p2panda/glib wiring directly and reads like every other feature module.

2. As a developer of a second Zodia client (the `mobile` branch, a future CLI, a bot), I want a `Send + 'static` API with no glib or relm4 dependency, so I can drive it from whatever runtime my platform uses.

3. As a Zodia contributor, I want the pipeline's `!Send` constraint to be an internal implementation detail of `zodia-sdk`, not something every consumer has to rediscover and work around.

4. As a Zodia user, I want subscribing to an aspect page and publishing an edit to behave identically regardless of which client I'm using, so the "network" the PRD name promises is actually one thing with multiple faces, not one app with private internals.

5. As a Zodia developer testing sync behaviour, I want to spin up two `ZodiaClient`s in one process and assert on the `StateEvent`s each produces, without a display server or a relm4 component tree — cheaper and faster than any test path available today.

6. As a Zodia developer adding a new UI feature that needs live updates for a key (e.g. a future "who's editing this right now" indicator), I want to call `.events()` and filter, not reach into `zodia-pipeline`/`zodia-sync` internals the way `app.rs` does today.

## Implementation Decisions

### Crate boundary

`zodia-pipeline`, `zodia-sync`, `zodia-net` stay exactly as they are — this is additive composition, not a rewrite. `zodia-sdk` depends on all three and is the *only* crate allowed to construct a `ZodiaNetwork` + `ZodiaSyncNode` + `ZodiaPipeline` triple; `app/` stops doing so directly.

### Public API sketch

```rust
// zodia-sdk/src/lib.rs

pub struct ZodiaClient {
    cmd_tx:    tokio::sync::mpsc::Sender<Command>,
    events_tx: tokio::sync::broadcast::Sender<StateEvent>,   // re-subscribe per listener via events()
    status_rx: tokio::sync::watch::Receiver<SyncStatus>,
}

pub struct ZodiaClientConfig {
    pub signing_key: ed25519_dalek::SigningKey,
    pub birth:       zodia_core::BirthData,
    pub data_dir:    std::path::PathBuf,
}

impl ZodiaClient {
    /// Spawns the dedicated thread, brings up network + sync + pipeline,
    /// and returns once the endpoint is live. Callable from any runtime.
    pub async fn connect(config: ZodiaClientConfig) -> Result<Self, ClientError>;

    /// Every materialised `StateEvent`, network-wide (not key-filtered —
    /// filtering by key is the caller's job, same as today's AppMsg fan-out).
    /// `broadcast` so the feed view, bell badge, and an open aspect page can
    /// each hold their own receiver off one internal stream.
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<StateEvent>;

    /// Caught-up-ness per peer, for a sync-status UI panel. `watch` because
    /// only the latest snapshot matters, not every transition.
    pub fn sync_status(&self) -> tokio::sync::watch::Receiver<SyncStatus>;

    pub async fn subscribe(&self, interp_key: &str) -> Result<(), ClientError>;
    pub async fn unsubscribe(&self, interp_key: &str) -> Result<(), ClientError>;

    pub async fn author(&self, interp_key: &str, body: String) -> Result<(), ClientError>;
    pub async fn edit(&self, interp_key: &str, base_rev: Hash,
                       crdt_update: Vec<u8>, affected_blocks: Vec<[u8; 16]>)
                       -> Result<(), ClientError>;
    pub async fn veto(&self, interp_key: &str, target_edit_op_id: Hash) -> Result<(), ClientError>;
    pub async fn affirm_rev(&self, interp_key: &str, target_rev: [u8; 32]) -> Result<(), ClientError>;
    pub async fn set_editor_presence(&self, interp_key: &str, joined: bool) -> Result<(), ClientError>;

    pub fn node_id(&self) -> zodia_net::PeerId;
}

#[derive(Debug, Clone)]
pub struct SyncStatus {
    pub peers_known:      usize,
    pub peers_caught_up:  usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("client thread is gone")]
    Disconnected,
    #[error("network: {0}")]
    Network(String),
    #[error("sync: {0}")]
    Sync(String),
}
```

Every `pub async fn` above is `send_command(Command::X { .. }, reply_tx).await` — a thin, mechanical translation of the existing `SyncPublishMsg` shape plus the request/reply pattern needed for `Result`-returning calls (today's fire-and-forget `try_send` loses errors on the floor; the SDK surface shouldn't).

### Threading model — the load-bearing decision

`ZodiaClient::connect` spawns one `std::thread` running a single-threaded tokio runtime + `LocalSet`, and everything `!Send` (the pipeline, the sync handles) lives there permanently. The command channel in and the broadcast channel out are the *only* crossing points, and both sides are ordinary `Send` types. This is the same shape `app.rs` already has (`glib::MainContext::default().spawn_local` + `mpsc::Sender<SyncPublishMsg>`) — the change is *who owns the thread*. Today it's borrowed from relm4's main loop, which is why nothing outside `app.rs` can use this code without also being a glib app. After this change the SDK owns its own thread unconditionally, and relm4 becomes a bridge, not the substrate.

### The relm4 bridge (what `app.rs` becomes)

```rust
// app/src/app.rs, replacing try_spawn_network + try_spawn_sync + the
// glib::spawn_local pump entirely:
let client = ZodiaClient::connect(config).await?;
let mut events = client.events();
let sender = sender.clone();
tokio::spawn(async move {
    while let Ok(event) = events.recv().await {
        sender.input(AppMsg::SyncStateEvent(event));
    }
});
model.zodia_client = Some(client);
```

`SyncPublishMsg` and the hand-rolled pump loop in `app.rs` (lines 2409-2480 today) are deleted outright — not deprecated alongside, deleted. Every current call site that does `tx.try_send(SyncPublishMsg::Publish(op))` becomes `client.author(...)`/`client.edit(...)`/etc., and gets a real `Result` back instead of silently dropping on a full channel.

### Event fan-out and backpressure

`broadcast::channel` means a slow subscriber (e.g. a backgrounded aspect page) can lag and miss events rather than back-pressuring the whole pipeline — `broadcast::error::RecvError::Lagged(n)` surfaces that explicitly rather than silently. Callers that need a full replay (the Sky feed's backfill) already go through `store.recent_feed_rows` for history and only need `events()` for what happens *after* they start listening — matches the existing `feed_view.rs` pattern (`__BackfillFeed` then live events), so no behaviour change there, just a cleaner source for the live half.

### Migration path

Purely additive at the wire/data level — no op format changes, no version cut, no `log_id`/topic changes (Phase C-2 stays exactly as shipped, `zodia-sdk` just calls its existing `subscribe`/`unsubscribe`/`publish_doc`). The only thing that moves is *which crate* owns the glib-coupled thread. Land `zodia-sdk` first with its own tests (below), then migrate `app.rs` call-site-by-call-site behind the new facade, deleting the old plumbing at the end rather than maintaining both.

## Testing Decisions

This is the biggest testing upgrade available in the near term: `zodia-sdk` tests need no display server, no relm4 component tree, no glib main loop — `#[tokio::test]` two `ZodiaClient`s against each other over loopback (same pattern `net/tests/channel.rs` already uses for `DirectChannel`), assert on `events()` output. Specifically:

- **Connect/disconnect**: `ZodiaClient::connect` succeeds, `node_id()` is stable, dropping the client tears down its thread (add a `Drop` impl that signals the command channel closed, or rely on `mpsc::Sender` drop + the internal loop's `select!` exiting — decide during implementation and assert on it, since a leaked thread per dropped client would be a real bug).
- **Subscribe → publish → receive**: two in-process clients, A subscribes to a key, B (also subscribed, or not — test both) publishes an edit, assert A's `events()` yields `StateEvent::DocEdited` for that key within a bounded wait.
- **Unsubscribed keys stay silent**: A never subscribes to a key; B publishes to it; assert A's `events()` does *not* yield anything for that key within a bounded window (regression test for Phase C-2's whole point).
- **Error surfacing**: calling any verb after the client's thread has died returns `ClientError::Disconnected` rather than hanging or panicking.
- **Backpressure**: a subscriber that doesn't poll `events()` for N published ops gets `Lagged`, not a stall of the publishing side.

## Progress notes

Shipped: the `zodia-sdk` crate as sketched above — `ZodiaClient::connect`/`events`/`sync_status`/`subscribe`/`unsubscribe`/`author`/`edit`/`veto`/`affirm_rev`/`set_editor_presence`, dedicated-thread + `LocalSet` internally, `oneshot`-backed `call()` helper giving every command a real `Result`. Three tests: connect/`node_id()` stability, thread teardown on drop, and a real two-`ZodiaClient` networked round trip (subscribe on both sides, publish an edit on one, observe `StateEvent::DocEdited` on the other over real iroh/p2panda transport — no mocking).

That round-trip test caught a genuine pre-existing bug while being written: `p2panda_store::topics::TopicStore::associate(topic, author, log_id)` was never being called anywhere in `zodia-sync`, for *any* topic, including the legacy global one — so `topics_v1` stayed empty and a peer's catch-up query ("local topic logs retrieved") could never find anything to serve, regardless of which topic model was in use. Only an already-open live sync session happened to mask this (direct in-memory forwarding, independent of `topics_v1`), which is exactly the kind of narrow, timing-dependent path Phase C-2's shorter-lived per-key topic subscriptions made much more likely to miss. Fixed in `sync/src/lib.rs::publish_bytes` — `associate` now runs inside the same transaction as `insert_operation`, before commit (its own `self.tx(..)` call requires an already-open transaction, same constraint `insert_operation` has). This fix benefits the already-shipped Phase C-2 log-splitting too, not just the SDK.

**`app.rs` migration, done.** The bridge sketch above was verified against the real relm4 message flow, with two real gaps found and fixed before it could work at all:

1. **`ZodiaClient::connect` always spawned its own `ZodiaNetwork`.** `app.rs` also needs `ZodiaNetwork` directly, for Tier-1 consent/chat/AV — capabilities this SDK doesn't cover and was never meant to (see "Out of Scope"). Two independent `ZodiaNetwork`s under the same signing key would have been wasteful and wrong (duplicate iroh endpoints, duplicate discovery/announce traffic, same identity racing itself). Added `ZodiaClient::attach(&net, signing_key, data_dir)`, which reuses an already-running `ZodiaNetwork`'s `Endpoint`/`Gossip` instead of spawning new ones — `app.rs` keeps owning and draining its own `ZodiaNetwork` for Tier-1 exactly as before; only the sync/pipeline layer moved onto the SDK. `run()`'s internals were split around a `NetworkSource::{Owned, Attached}` enum; the `Owned` case has to keep the `ZodiaNetwork` bound for the connection's whole lifetime, not just setup, since dropping it early tears down discovery/mDNS.
2. **`sync_status()` only exposed aggregate counts.** `app.rs`'s existing "Sync activity" panel needs raw per-peer `SyncStarted`/`SyncFinished`/`Failed` with `remote_pk` and `received_ops`, not just `{peers_known, peers_caught_up}`. Added `SyncLifecycleEvent` + `sync_lifecycle_events()`, broadcast alongside (not instead of) the aggregate.
3. **Two legacy ops had no `ZodiaClient` method at all** (initially): `InterpOp::Affirm` and `InterpOp::RespondTo`. Added `affirm()`/`respond_to()` to unblock `app.rs`'s `AppMsg::AffirmInterp`/`SubmitResponse` handlers — then, once the migration was otherwise complete, a full-crate reachability check (`grep` for every AppMsg variant across all of `app/src/*.rs`, not just the two files touched) found `AffirmInterp`, `SubmitInterp`, `SubmitResponse`, and the `ShareInterp` they chained to were **never sent by any live widget** — the legacy "competing whole interpretations" write UI was already fully superseded by the collaborative-doc model (author→edit, affirm→affirm_rev, respond→inline editing) and had no UI surface left to trigger from. Deleted all four `AppMsg` variants and handlers from `app.rs`, and correspondingly `affirm()`/`respond_to()` + their `Command` variants from this SDK, since nothing in the workspace called or tested them once the dead handlers were gone. `author()` and `revoke()` were *not* removed — both have real call sites (`AppMsg::ShareInterp` before deletion; `AppMsg::SubmitRevoke`, which turned out to be very much alive via a working "Revoke and delete" button on `feed_view.rs`'s activity cards) and real cucumber coverage (`author_propagation.feature`, `revoke_propagation.feature`).

This second pass is also the correction to the finding that started this whole migration: an earlier, narrower audit (grepping only `aspect_view.rs` and `app.rs`) concluded `SubmitRevoke` had "zero UI trigger anywhere" — wrong, just an incomplete grep that missed `feed_view.rs`. The revoke button was there the whole time. The SDK migration was still the right call independent of that correction (it fixed two real architectural gaps, §1–2 above), but the specific capability gap that motivated starting it didn't actually exist.

Every one of the 9 `SyncPublishMsg::{Publish,PublishDoc}` call sites in `app.rs` now calls the matching `ZodiaClient` method directly and `.await`s a real `Result` (logged on error) instead of `try_send`-and-forget. `subscribe_own_chart_keys` awaits `client.subscribe` per key instead of firing into a channel. `SyncPublishMsg`, `try_spawn_sync`'s old body, and the direct `zodia_sync`/`zodia_pipeline` imports in `app.rs` are gone — `app/Cargo.toml` no longer depends on `zodia-sync` at all.

Verified two ways: `cargo build --workspace` clean (one warning, an unused import, fixed), and the compiled binary actually launched under a real X11/Wayland session (`XDG_DATA_HOME` pointed at a scratch dir to avoid touching real user data) — completed setup, spawned the network via the new `attach()` path twice (cold-start `init()` path and the `ConfirmBirth` path), wrote to `sync_log.db`/`interpretations.db` in the scratch dir, ran stably for several minutes with no panics or errors beyond the pre-existing benign `Endpoint dropped without calling Endpoint::close` warning already seen throughout this project's test suite, then shut down cleanly on `SIGTERM`.

Two new `zodia-sdk` unit tests cover `attach()` specifically: `attach_reuses_an_existing_zodia_networks_identity` (node_id matches the shared network's) and `attach_round_trip_matches_connect_round_trip` (two independently-attached clients still converge on an edit — proves `attach()` carries real traffic, not just constructs).

**Still open:** the `Lagged` backpressure test from Testing Decisions above.

## Out of Scope

- **Changing `zodia-net`/`zodia-sync`/`zodia-pipeline` internals.** Pure composition layer; if those crates need changes, that's their own PRD.
- **A stable, versioned public API for external (non-Zodia-org) consumers.** This PRD gets the *shape* right and gets `app.rs` off bespoke plumbing; publishing to crates.io / API-stability guarantees for third parties is a later, separate decision.
- **The `mobile` branch itself.** This PRD doesn't port or unblock it, it just removes the specific "you'd have to copy glib-coupled internals" obstacle it would otherwise hit.
- **Phase C-2's remaining UI wiring** (on-demand page subscribe, grace-period unsubscribe — see `docs/prd/granular-topic-subscription.md`). `zodia-sdk` exposes `subscribe`/`unsubscribe`; deciding *when* `app.rs` calls them is that PRD's open work, unchanged by this one.

## Further Notes

**Why this is the right next move given C-2.** Implementing Phase C-2 just added exactly the verb set (`Subscribe`/`Unsubscribe` alongside `Publish`/`PublishDoc`) that makes a clean facade obvious — `SyncPublishMsg` today *is* the SDK's command enum in miniature, just trapped inside `app/` and untyped-error (`try_send` swallows failures). Promoting it now, while the shape is fresh, costs less than doing it after more call sites accrete around the current pattern.

**Why "reference implementation" framing matters.** If a third party — or the `mobile` branch — is ever going to point at Zodia's desktop app as proof of what the network supports, the app needs to visibly *consume* a library rather than *be* the only implementation of the protocol. `zodia-sdk` existing as a real crate boundary, with its own tests that don't require relm4, is the concrete evidence of that — not a claim in a README.
