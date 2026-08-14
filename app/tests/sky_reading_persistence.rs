//! Cucumber harness for `tests/sky-reading-features/sky_reading_persistence
//! .feature` — proves a published Sky-feed reading shows up immediately and
//! survives a restart with the same identity/data directory, driving one
//! real app instance twice through its own message layer (see
//! `src/screenshot_script.rs`).
//!
//! Needs a display, so the scenario only runs when `ZODIA_UI_TESTS` is set
//! — otherwise this test binary prints a skip notice and exits green,
//! keeping plain `cargo test` working everywhere. Unlike `screenshots.rs`
//! this doesn't need real networking (single instance, no peer exchange),
//! so it's gated by its own env var rather than `ZODIA_SCREENSHOTS_OUT`.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use cucumber::{World as _, given, then, when};
use tempfile::TempDir;

#[derive(cucumber::World)]
struct SkyWorld {
    out_dir:  TempDir,
    data_dir: TempDir,
    seed:     String,
}

impl std::fmt::Debug for SkyWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkyWorld").field("out_dir", &self.out_dir.path()).finish()
    }
}

impl Default for SkyWorld {
    fn default() -> Self {
        Self {
            out_dir:  TempDir::new().expect("temp out dir"),
            data_dir: TempDir::new().expect("temp data dir"),
            seed:     String::new(),
        }
    }
}

/// Write the named script template with `{OUT}` substituted, into the
/// instance's data dir (same substitution `screenshots.rs` uses).
fn prepare_script(world: &SkyWorld, template: &str) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/screenshot-scripts")
        .join(template);
    let body = std::fs::read_to_string(&src)
        .unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
    let body = body.replace("{OUT}", &world.out_dir.path().display().to_string());
    let dst = world.data_dir.path().join(template);
    std::fs::write(&dst, body).expect("write script");
    dst
}

/// Run one app instance to completion against `world`'s shared data dir.
fn run_instance(world: &SkyWorld, template: &str) {
    let script = prepare_script(world, template);
    let log_path = world.out_dir.path().join(format!("{template}.log"));
    let log = std::fs::File::create(&log_path).expect("create instance log");
    let mut child = Command::new("dbus-run-session")
        .arg("--")
        .arg(env!("CARGO_BIN_EXE_zodia"))
        .env("XDG_DATA_HOME", world.data_dir.path())
        .env("ZODIA_SEED_DEMO", &world.seed)
        .env("ZODIA_SCREENSHOT_SCRIPT", &script)
        .stdout(log.try_clone().expect("clone log handle"))
        .stderr(log)
        .spawn()
        .expect("spawn zodia instance (is dbus-run-session installed?)");

    let deadline = Instant::now() + Duration::from_secs(2 * 60);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "instance exited with {status} — see {}",
                    log_path.display());
                break;
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("instance did not finish within the deadline — see {}", log_path.display());
            }
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn read_dump(world: &SkyWorld, name: &str) -> String {
    let path = world.out_dir.path().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const READING_TEXT: &str = "The stars ripple with unusual clarity today.";

#[given(expr = "a lone instance {string} born in Oslo")]
async fn lone_instance(world: &mut SkyWorld, name: String) {
    world.seed = format!("{name};2451545.0;59.9139;10.7461");
}

#[when("she publishes a reading for the first active sky transit")]
async fn publish_reading(world: &mut SkyWorld) {
    run_instance(world, "publish_before_restart.script");
}

#[then("the Sky feed shows the reading immediately")]
async fn shows_immediately(world: &mut SkyWorld) {
    let before = read_dump(world, "before_publish.txt");
    assert!(before.contains("No reading yet"),
        "expected the pre-publish dump to show the empty state, got:\n{before}");

    let after = read_dump(world, "after_publish.txt");
    assert!(after.contains(READING_TEXT),
        "Sky feed did not show the published reading right after publishing:\n{after}");
    assert!(!after.contains("No reading yet"),
        "Sky feed still shows the empty-state text after publishing:\n{after}");
}

#[when("the instance restarts using the same data directory")]
async fn restart_instance(world: &mut SkyWorld) {
    run_instance(world, "after_restart.script");
}

#[then("the Sky feed still shows the reading")]
async fn still_shows_after_restart(world: &mut SkyWorld) {
    let after = read_dump(world, "after_restart.txt");
    assert!(after.contains(READING_TEXT),
        "Sky feed reverted to the empty state after restarting:\n{after}");
    assert!(!after.contains("No reading yet"),
        "Sky feed shows the empty-state text after restarting:\n{after}");
}

#[tokio::main]
async fn main() {
    if std::env::var("ZODIA_UI_TESTS").is_err() {
        eprintln!(
            "sky_reading_persistence: skipped (set ZODIA_UI_TESTS=1 to run \
             — needs a display, no network required)"
        );
        return;
    }
    SkyWorld::run("tests/sky-reading-features").await;
}
