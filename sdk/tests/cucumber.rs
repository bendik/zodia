//! Cucumber step definitions for `sdk/tests/features/*.feature`.
//!
//! Scenarios exercise real `ZodiaClient`s over real iroh/p2panda transport
//! — no mocking, same approach as `src/lib.rs`'s unit tests. Each scenario
//! spins up 1-2 real network nodes, so this suite is slower than a typical
//! unit test file (seconds, not milliseconds) but proves the actual
//! data-flow guarantees these PRDs promise, not a simulation of them.
//!
//! These scenarios cover what `zodia-sdk` can prove end-to-end: op
//! propagation and materialisation. They do not cover app-layer behavior
//! (UI rendering, notification badging, veto enforcement) — see
//! `docs/testing/coverage-and-bdd-scenarios.md` for that gap.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use cucumber::{World as _, given, then, when};
use ed25519_dalek::SigningKey;
use p2panda_core::Hash;
use rand_core::OsRng;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;

use zodia_sdk::{StateEvent, ZodiaClient, ZodiaClientConfig};

#[derive(cucumber::World, Default)]
struct ZodiaWorld {
    clients:   HashMap<String, ZodiaClient>,
    events:    HashMap<String, broadcast::Receiver<StateEvent>>,
    tmp_dirs:  Vec<TempDir>,
}

// `cucumber::World::run` requires `Debug` (used to log World state on step
// failure); neither `ZodiaClient` nor `broadcast::Receiver` derive it, so
// this just names which peers are live rather than dumping their internals.
impl fmt::Debug for ZodiaWorld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZodiaWorld")
            .field("peers", &self.clients.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[given(expr = "a peer named {string} connected to the network")]
async fn peer_connected(world: &mut ZodiaWorld, name: String) {
    let tmp = TempDir::new().expect("create temp data dir");
    let config = ZodiaClientConfig {
        signing_key: SigningKey::generate(&mut OsRng),
        birth:       zodia_core::birth_from_coords(2_451_545.0, 59.9, 10.7, 9),
        data_dir:    tmp.path().to_path_buf(),
    };
    let client = ZodiaClient::connect(config).await.expect("client connects");
    world.events.insert(name.clone(), client.events());
    world.clients.insert(name, client);
    world.tmp_dirs.push(tmp);
}

#[given(expr = "{string} is subscribed to {string}")]
async fn peer_subscribes(world: &mut ZodiaWorld, name: String, key: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .subscribe(&key)
        .await
        .expect("subscribe succeeds");
}

// Registered for both Given and When: an "And" step's effective type is
// whatever the nearest preceding explicit Given/When/Then was, and this
// step appears after a When in "re-touching" scenarios (see
// grace_period_unsubscribe.feature's second scenario).
#[given(regex = r#"^"([^"]+)" touches subscription to "([^"]+)" with a grace period of (\d+) seconds?$"#)]
#[when(regex = r#"^"([^"]+)" touches subscription to "([^"]+)" with a grace period of (\d+) seconds?$"#)]
async fn peer_touches_subscription(world: &mut ZodiaWorld, name: String, key: String, grace_secs: u64) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .touch_subscription(&key, Duration::from_secs(grace_secs))
        .await
        .expect("touch_subscription succeeds");
}

#[when(regex = r"^(\d+) seconds? (?:pass|passes)$")]
async fn time_passes(_world: &mut ZodiaWorld, secs: u64) {
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

#[when(expr = "{string} edits {string}")]
async fn peer_edits(world: &mut ZodiaWorld, name: String, key: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .edit(&key, Hash::from_bytes([0u8; 32]), vec![1, 2, 3], vec![[9u8; 16]])
        .await
        .expect("edit succeeds");
}

#[when(expr = "{string} affirms the current revision of {string}")]
async fn peer_affirms(world: &mut ZodiaWorld, name: String, key: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .affirm_rev(&key, [0u8; 32])
        .await
        .expect("affirm succeeds");
}

#[when(expr = "{string} revokes a contribution")]
async fn peer_revokes(world: &mut ZodiaWorld, name: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .revoke(Hash::from_bytes([3u8; 32]))
        .await
        .expect("revoke succeeds");
}

#[then(expr = "{string} observes the revocation within {int} seconds")]
async fn observes_revocation(world: &mut ZodiaWorld, name: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::InterpRevoked { target_log_id, .. } if *target_log_id == Hash::from_bytes([3u8; 32]))
    }).await;
    assert!(seen, "{name} did not observe the revocation within {secs}s");
}

#[then(expr = "{string} observes a doc edit on {string} within {int} seconds")]
async fn observes_doc_edit(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::DocEdited { interp_key, .. } if *interp_key == key)
    }).await;
    assert!(seen, "{name} did not observe a doc edit on {key} within {secs}s");
}

#[then(expr = "{string} observes no doc edit on {string} within {int} seconds")]
async fn observes_no_doc_edit(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::DocEdited { interp_key, .. } if *interp_key == key)
    }).await;
    assert!(!seen, "{name} unexpectedly observed a doc edit on {key} — an unsubscribed key should not leak live updates");
}

#[then(expr = "{string} observes a doc affirmation on {string} within {int} seconds")]
async fn observes_doc_affirmation(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::DocAffirmed { interp_key, .. } if *interp_key == key)
    }).await;
    assert!(seen, "{name} did not observe a doc affirmation on {key} within {secs}s");
}

/// Poll `name`'s event stream up to `secs` for an event matching
/// `predicate`. Returns `true` if found before the deadline, `false` if
/// the deadline passed with no match — a bounded, honest way to assert
/// absence (see `observes_no_doc_edit`) without waiting forever.
async fn wait_for(
    world:     &mut ZodiaWorld,
    name:      &str,
    secs:      u64,
    predicate: impl Fn(&StateEvent) -> bool,
) -> bool {
    let events = world.events.get_mut(name).unwrap_or_else(|| panic!("no peer named {name}"));
    let result = timeout(Duration::from_secs(secs), async {
        loop {
            let event = events.recv().await.expect("events channel closed unexpectedly");
            if predicate(&event) {
                return;
            }
        }
    }).await;
    result.is_ok()
}

#[tokio::main]
async fn main() {
    // Each scenario spins up real network nodes (iroh endpoints, mDNS,
    // gossip) — running scenarios concurrently (cucumber's default)
    // contends for those real resources within one test process and
    // produces timing flakiness unrelated to the behavior under test.
    // These are integration tests, not unit tests; run them serially.
    ZodiaWorld::cucumber()
        .max_concurrent_scenarios(1)
        .run_and_exit("tests/features")
        .await;
}
