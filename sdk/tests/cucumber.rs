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
use p2panda_core::{Hash, SigningKey as PandaSigningKey};
use rand_core::OsRng;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;

use zodia_sdk::{CircleAccess, CircleId, StateEvent, ZodiaClient, ZodiaClientConfig};

#[derive(cucumber::World, Default)]
struct ZodiaWorld {
    clients:    HashMap<String, ZodiaClient>,
    events:     HashMap<String, broadcast::Receiver<StateEvent>>,
    tmp_dirs:   Vec<TempDir>,
    /// (signing_key, data_dir) per peer name, kept so a "reconnects"
    /// step can bring the same identity + local storage back up after
    /// a "disconnects" step drops it.
    identities:       HashMap<String, (SigningKey, std::path::PathBuf)>,
    last_prune_count: Option<u64>,
    /// The circle most recently created in the scenario — every scenario
    /// so far only ever deals with one circle at a time, same simplicity
    /// as `last_prune_count`.
    last_circle: Option<CircleId>,
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

// Registered for both Given and When — see edit_survives_restart.feature,
// where a second peer connects mid-scenario after a When step.
#[given(expr = "a peer named {string} connected to the network")]
#[when(expr = "a peer named {string} connected to the network")]
async fn peer_connected(world: &mut ZodiaWorld, name: String) {
    let tmp = TempDir::new().expect("create temp data dir");
    let signing_key = SigningKey::generate(&mut OsRng);
    let data_dir = tmp.path().to_path_buf();
    let config = ZodiaClientConfig {
        signing_key: signing_key.clone(),
        birth:       zodia_core::birth_from_coords(2_451_545.0, 59.9, 10.7, 9),
        data_dir:    data_dir.clone(),
    };
    let client = ZodiaClient::connect(config).await.expect("client connects");
    // Self-tag as a test peer so real Zodia instances on the same LAN (mDNS
    // reaches the whole network, not just this test process) can filter
    // this ephemeral identity out of their discovery UI instead of
    // treating it as a real person to connect with — see
    // `zodia_sdk::TEST_PEER_DISPLAY_NAME`'s doc comment.
    client.set_display_name(zodia_sdk::TEST_PEER_DISPLAY_NAME.to_string()).await
        .expect("set_display_name succeeds");
    world.events.insert(name.clone(), client.events());
    world.identities.insert(name.clone(), (signing_key, data_dir));
    world.clients.insert(name, client);
    world.tmp_dirs.push(tmp);
}

#[when(expr = "{string} disconnects")]
async fn peer_disconnects(world: &mut ZodiaWorld, name: String) {
    world.clients.remove(&name).unwrap_or_else(|| panic!("no peer named {name}"));
    world.events.remove(&name);
    // Identity + data_dir stay in world.identities — "reconnects" needs them.
}

#[when(expr = "{string} reconnects using the same identity and data directory")]
async fn peer_reconnects(world: &mut ZodiaWorld, name: String) {
    let (signing_key, data_dir) = world.identities.get(&name)
        .unwrap_or_else(|| panic!("no prior identity for peer {name}"))
        .clone();
    let config = ZodiaClientConfig {
        signing_key,
        birth: zodia_core::birth_from_coords(2_451_545.0, 59.9, 10.7, 9),
        data_dir,
    };
    let client = ZodiaClient::connect(config).await.expect("client reconnects");
    client.set_display_name(zodia_sdk::TEST_PEER_DISPLAY_NAME.to_string()).await
        .expect("set_display_name succeeds");
    world.events.insert(name.clone(), client.events());
    world.clients.insert(name, client);
}

// Registered for both Given and When: an "And" step's effective type is
// whatever the nearest preceding explicit Given/When/Then was, and this
// step is used both up front (Given) and after a reconnect (When) — see
// edit_survives_restart.feature.
#[given(expr = "{string} is subscribed to {string}")]
#[when(expr = "{string} is subscribed to {string}")]
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

#[given(regex = r"^(\d+) seconds? (?:pass|passes)$")]
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

#[when(expr = "{string} starts editing {string}")]
async fn peer_starts_editing(world: &mut ZodiaWorld, name: String, key: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .set_editor_presence(&key, true)
        .await
        .expect("set_editor_presence(true) succeeds");
}

#[when(expr = "{string} stops editing {string}")]
async fn peer_stops_editing(world: &mut ZodiaWorld, name: String, key: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .set_editor_presence(&key, false)
        .await
        .expect("set_editor_presence(false) succeeds");
}

#[then(expr = "{string} observes editor presence joined on {string} within {int} seconds")]
async fn observes_presence_joined(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::EditorPresenceChanged { interp_key, joined: true, .. } if *interp_key == key)
    }).await;
    assert!(seen, "{name} did not observe editor-presence-joined on {key} within {secs}s");
}

#[then(expr = "{string} observes editor presence left on {string} within {int} seconds")]
async fn observes_presence_left(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::EditorPresenceChanged { interp_key, joined: false, .. } if *interp_key == key)
    }).await;
    assert!(seen, "{name} did not observe editor-presence-left on {key} within {secs}s");
}

#[when(expr = "{string} vetoes an edit on {string}")]
async fn peer_vetoes(world: &mut ZodiaWorld, name: String, key: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .veto(&key, Hash::from_bytes([5u8; 32]))
        .await
        .expect("veto succeeds");
}

#[then(expr = "{string} observes a veto proposal on {string} within {int} seconds")]
async fn observes_veto_proposal(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::DocVetoProposed { interp_key, .. } if *interp_key == key)
    }).await;
    assert!(seen, "{name} did not observe a veto proposal on {key} within {secs}s");
}

// No subscription needed: `author()` is legacy log-0 content, which
// propagates over the always-on global topic (see `ZodiaClient::revoke`'s
// doc comment) — every connected peer receives it regardless of what
// they're subscribed to.
#[when(regex = r#"^"([^"]+)" authors (\d+) interpretations rapidly$"#)]
async fn peer_authors_many(world: &mut ZodiaWorld, name: String, count: u64) {
    let client = world.clients.get(&name).unwrap_or_else(|| panic!("no peer named {name}"));
    for i in 0..count {
        client.author(&format!("natal:backpressure_{i}"), format!("body {i}")).await
            .expect("author succeeds");
    }
}

#[when(regex = r#"^"([^"]+)" sets their display name to "([^"]+)"$"#)]
async fn peer_sets_display_name(world: &mut ZodiaWorld, name: String, display_name: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .set_display_name(display_name)
        .await
        .expect("set_display_name succeeds");
}

// Registered for both Then and When: this file's other dual-registered
// steps are reused as an "And" following either keyword; this scenario's
// second display-name change is asserted mid-scenario after a "When".
#[then(regex = r#"^"([^"]+)" observes "([^"]+)" set their display name to "([^"]+)" within (\d+) seconds?$"#)]
#[when(regex = r#"^"([^"]+)" observes "([^"]+)" set their display name to "([^"]+)" within (\d+) seconds?$"#)]
async fn observes_display_name(
    world: &mut ZodiaWorld, observer: String, author: String, display_name: String, secs: u64,
) {
    let (author_signing_key, _) = world.identities.get(&author)
        .unwrap_or_else(|| panic!("no prior identity for peer {author}"))
        .clone();
    let author_verifying_key = PandaSigningKey::from_bytes(author_signing_key.as_bytes()).verifying_key();
    let seen = wait_for(world, &observer, secs, |event| {
        matches!(
            event,
            StateEvent::DisplayNameSet { by, name, .. }
                if *by == author_verifying_key && name == &display_name
        )
    }).await;
    assert!(seen, "{observer} did not observe {author} set their display name to {display_name:?} within {secs}s");
}

#[when(expr = "{string} authors an interpretation on {string}")]
async fn peer_authors(world: &mut ZodiaWorld, name: String, key: String) {
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .author(&key, "a test interpretation body".to_string())
        .await
        .expect("author succeeds");
}

#[then(expr = "{string} observes the authored interpretation on {string} within {int} seconds")]
async fn observes_authored(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::InterpAuthored { interp_key, .. } if *interp_key == key)
    }).await;
    assert!(seen, "{name} did not observe the authored interpretation on {key} within {secs}s");
}

#[then(expr = "{string} observes no authored interpretation on {string} within {int} seconds")]
async fn observes_no_authored(world: &mut ZodiaWorld, name: String, key: String, secs: u64) {
    let seen = wait_for(world, &name, secs, |event| {
        matches!(event, StateEvent::InterpAuthored { interp_key, .. } if *interp_key == key)
    }).await;
    assert!(
        !seen,
        "{name} unexpectedly observed the interpretation on {key} — a peer never invited to the circle should not receive readable content"
    );
}

#[given(expr = "{string} creates a circle")]
async fn peer_creates_circle(world: &mut ZodiaWorld, name: String) {
    let circle_id = world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .create_circle(vec![])
        .await
        .expect("create_circle succeeds");
    world.last_circle = Some(circle_id);
}

async fn invite_with_access(world: &mut ZodiaWorld, name: String, invitee: String, access: CircleAccess<()>) {
    let circle_id = world.last_circle.expect("a circle must be created before inviting to it");
    let (invitee_signing_key, _) = world.identities.get(&invitee)
        .unwrap_or_else(|| panic!("no prior identity for peer {invitee}"))
        .clone();
    let invitee_verifying_key = PandaSigningKey::from_bytes(invitee_signing_key.as_bytes()).verifying_key();
    let client = world.clients.get(&name).unwrap_or_else(|| panic!("no peer named {name}"));

    // The invitee's key bundle reaches us via the circle directory topic's
    // own catch-up sync, same real-network settle time documented for
    // opening a second brand-new topic between a peer pair (up to ~25s) —
    // poll rather than guess a fixed sleep, matching this suite's
    // established real-elapsed-time-over-synthetic-backdating approach.
    let result = timeout(Duration::from_secs(25), async {
        loop {
            match client.invite_to_circle(circle_id, invitee_verifying_key, access.clone()).await {
                Ok(()) => return,
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    }).await;
    result.unwrap_or_else(|_| panic!("{name} could not invite {invitee} to the circle within 25s (key bundle never discovered)"));
}

// Registered for both Given and When: circle_sharing.feature's second
// scenario uses this as an "And" following a "Given", same reasoning as
// this file's other dual-registered steps.
#[given(expr = "{string} invites {string} to the circle")]
#[when(expr = "{string} invites {string} to the circle")]
async fn peer_invites_to_circle(world: &mut ZodiaWorld, name: String, invitee: String) {
    invite_with_access(world, name, invitee, CircleAccess::read()).await;
}

#[given(expr = "{string} invites {string} to the circle with write access")]
#[when(expr = "{string} invites {string} to the circle with write access")]
async fn peer_invites_to_circle_with_write(world: &mut ZodiaWorld, name: String, invitee: String) {
    invite_with_access(world, name, invitee, CircleAccess::write()).await;
}

// Stands in for the invitee learning the circle's id out-of-band (a real
// invite notification/link — out of scope here, see
// ZodiaSyncNode::open_circle's doc comment) and then joining. Registered
// for both Given and When for the same reason as this file's other
// dual-registered steps.
#[given(expr = "{string} joins the circle")]
#[when(expr = "{string} joins the circle")]
async fn peer_joins_circle(world: &mut ZodiaWorld, name: String) {
    let circle_id = world.last_circle.expect("a circle must be created before joining it");
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .open_circle(circle_id)
        .await
        .expect("open_circle succeeds");
}

#[when(expr = "{string} revokes {string} from the circle")]
async fn peer_revokes_from_circle(world: &mut ZodiaWorld, name: String, member: String) {
    let circle_id = world.last_circle.expect("a circle must be created before revoking from it");
    let (member_signing_key, _) = world.identities.get(&member)
        .unwrap_or_else(|| panic!("no prior identity for peer {member}"))
        .clone();
    let member_verifying_key = PandaSigningKey::from_bytes(member_signing_key.as_bytes()).verifying_key();
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .revoke_from_circle(circle_id, member_verifying_key)
        .await
        .expect("revoke_from_circle succeeds");
}

#[then(expr = "{string} sees {string} as a write member of the circle")]
async fn sees_write_member(world: &mut ZodiaWorld, name: String, member: String) {
    let circle_id = world.last_circle.expect("a circle must be created before checking its members");
    let (member_signing_key, _) = world.identities.get(&member)
        .unwrap_or_else(|| panic!("no prior identity for peer {member}"))
        .clone();
    let member_verifying_key = PandaSigningKey::from_bytes(member_signing_key.as_bytes()).verifying_key();
    let members = world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .circle_members(circle_id)
        .await
        .expect("circle_members succeeds");
    let found = members.iter().find(|(vk, _)| *vk == member_verifying_key);
    match found {
        Some((_, access)) => assert!(access.is_write(), "{member} is a member but not at write access: {access:?}"),
        None => panic!("{member} is not listed as a circle member at all"),
    }
}

#[then(expr = "{string} no longer sees {string} as a member of the circle")]
async fn no_longer_sees_member(world: &mut ZodiaWorld, name: String, member: String) {
    let circle_id = world.last_circle.expect("a circle must be created before checking its members");
    let (member_signing_key, _) = world.identities.get(&member)
        .unwrap_or_else(|| panic!("no prior identity for peer {member}"))
        .clone();
    let member_verifying_key = PandaSigningKey::from_bytes(member_signing_key.as_bytes()).verifying_key();
    let members = world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .circle_members(circle_id)
        .await
        .expect("circle_members succeeds");
    assert!(
        !members.iter().any(|(vk, _)| *vk == member_verifying_key),
        "{member} is still listed as a circle member after being revoked"
    );
}

#[when(expr = "{string} shares an interpretation on {string} to the circle")]
async fn peer_shares_to_circle(world: &mut ZodiaWorld, name: String, key: String) {
    let circle_id = world.last_circle.expect("a circle must be created before sharing to it");
    world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .share_interp_to_circle(circle_id, &key, "a private test interpretation body".to_string())
        .await
        .expect("share_interp_to_circle succeeds");
}

#[when(regex = r#"^"([^"]+)" prunes local storage with a retention of (\d+) seconds?$"#)]
async fn peer_prunes(world: &mut ZodiaWorld, name: String, retention_secs: u64) {
    let removed = world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .prune(Duration::from_secs(retention_secs))
        .await
        .expect("prune succeeds");
    world.last_prune_count = Some(removed);
}

#[then(expr = "pruning removed at least {int} operation")]
async fn pruning_removed_at_least(world: &mut ZodiaWorld, min: u64) {
    let removed = world.last_prune_count.expect("prune was called before this assertion");
    assert!(removed >= min, "expected at least {min} operations pruned, got {removed}");
}

#[then(expr = "pruning removed {int} operations")]
async fn pruning_removed_exactly(world: &mut ZodiaWorld, expected: u64) {
    let removed = world.last_prune_count.expect("prune was called before this assertion");
    assert_eq!(removed, expected);
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

// Registered for both Then and When: pruning.feature uses this mid-scenario
// as an "And" following a "When", not as the scenario's own "Then".
#[then(expr = "{string} observes a doc edit on {string} within {int} seconds")]
#[when(expr = "{string} observes a doc edit on {string} within {int} seconds")]
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

#[then(expr = "{string} observes a lagged events error within {int} seconds")]
async fn observes_lagged(world: &mut ZodiaWorld, name: String, secs: u64) {
    let events = world.events.get_mut(&name).unwrap_or_else(|| panic!("no peer named {name}"));
    let result = timeout(Duration::from_secs(secs), async {
        loop {
            match events.recv().await {
                Err(broadcast::error::RecvError::Lagged(_)) => return true,
                Err(broadcast::error::RecvError::Closed) => return false,
                Ok(_) => continue,
            }
        }
    }).await;
    assert!(
        result.unwrap_or(false),
        "{name} did not observe a Lagged events error within {secs}s — either backpressure isn't working or the channel closed"
    );
}

#[then(expr = "{string} has caught up with at least {int} peer")]
async fn has_caught_up_with_at_least(world: &mut ZodiaWorld, name: String, min_peers: usize) {
    let status = world.clients.get(&name)
        .unwrap_or_else(|| panic!("no peer named {name}"))
        .sync_status();
    let caught_up = status.borrow().peers_caught_up;
    assert!(
        caught_up >= min_peers,
        "{name}'s sync_status shows {caught_up} peers caught up, expected at least {min_peers}"
    );
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
