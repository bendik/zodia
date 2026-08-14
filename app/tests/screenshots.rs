//! Cucumber harness for `tests/features/screenshots.feature` — regenerates
//! the four listing screenshots from two live app instances driven through
//! their own message layer (see `src/screenshot_script.rs`).
//!
//! Needs a display and real UDP networking, so the scenario only runs when
//! `ZODIA_SCREENSHOTS_OUT` names an output directory — otherwise this test
//! binary prints a skip notice and exits green, keeping plain `cargo test`
//! working everywhere. Run via `scripts/generate-screenshots.sh`.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime};

use cucumber::{World as _, given, then, when};
use tempfile::TempDir;

/// One app instance waiting to be launched (Given) or running (When).
struct Instance {
    child:     Option<Child>,
    data_dir:  TempDir,
    script:    PathBuf,
    seed:      String,
}

#[derive(cucumber::World, Default)]
struct ShotWorld {
    out_dir:   PathBuf,
    started:   Option<SystemTime>,
    instances: Vec<Instance>,
}

impl std::fmt::Debug for ShotWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShotWorld")
            .field("out_dir", &self.out_dir)
            .field("instances", &self.instances.len())
            .finish()
    }
}

fn out_dir() -> PathBuf {
    PathBuf::from(std::env::var("ZODIA_SCREENSHOTS_OUT").expect("checked in main"))
}

/// Write the named script template with `{OUT}` substituted, into the
/// instance's own temp dir.
fn prepare_script(template: &str, data_dir: &TempDir) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/screenshot-scripts")
        .join(template);
    let body = std::fs::read_to_string(&src)
        .unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
    let body = body.replace("{OUT}", &out_dir().display().to_string());
    let dst = data_dir.path().join(template);
    std::fs::write(&dst, body).expect("write script");
    dst
}

fn prepare_instance(world: &mut ShotWorld, template: &str, seed: &str) {
    if world.started.is_none() {
        world.started = Some(SystemTime::now());
        world.out_dir = out_dir();
        std::fs::create_dir_all(&world.out_dir).expect("create output dir");
    }
    let data_dir = TempDir::new().expect("temp data dir");
    let script = prepare_script(template, &data_dir);
    world.instances.push(Instance {
        child: None,
        data_dir,
        script,
        seed: seed.to_string(),
    });
}

#[given(expr = "a primary instance {string} born in Oslo")]
async fn primary_instance(world: &mut ShotWorld, name: String) {
    prepare_instance(world, "astrid.script", &format!("{name};2451545.0;59.9139;10.7461"));
}

#[given(expr = "a secondary instance {string} born in Bergen")]
async fn secondary_instance(world: &mut ShotWorld, name: String) {
    prepare_instance(world, "luna.script", &format!("{name};2451545.0;60.3930;5.3242"));
}

#[when("both instances run their screenshot scripts to completion")]
async fn run_instances(world: &mut ShotWorld) {
    // Under a tiling compositor the window would get tiled to whatever the
    // layout dictates and the 1600x1240 the script requests never sticks —
    // shots then come out workspace-sized. On sway, float the app windows
    // for the duration (a for_window rule only affects windows opened after
    // it, so this never touches existing windows). Other environments are
    // expected to honor the requested size (headless compositors do).
    if std::env::var("SWAYSOCK").is_ok() {
        for criterion in ["[app_id=\"io.github.bendik.Zodia\"]", "[class=\"zodia\"]"] {
            let _ = Command::new("swaymsg")
                .arg(format!("for_window {criterion} floating enable"))
                .status();
        }
    }

    let out_dir = world.out_dir.clone();
    for inst in &mut world.instances {
        // Each instance under its own session bus: GApplication is
        // single-instance per bus, and this also keeps the run from
        // activating a real Zodia the developer has open.
        // Per-instance log files (named after the script) — two apps'
        // interleaved stdout is undebuggable otherwise.
        let log_path = out_dir.join(format!(
            "{}.log",
            inst.script.file_stem().and_then(|s| s.to_str()).unwrap_or("instance"),
        ));
        let log = std::fs::File::create(&log_path).expect("create instance log");
        let child = Command::new("dbus-run-session")
            .arg("--")
            .arg(env!("CARGO_BIN_EXE_zodia"))
            .env("XDG_DATA_HOME", inst.data_dir.path())
            .env("ZODIA_SEED_DEMO", &inst.seed)
            .env("ZODIA_SCREENSHOT_SCRIPT", &inst.script)
            .stdout(log.try_clone().expect("clone log handle"))
            .stderr(log)
            .spawn()
            .expect("spawn zodia instance (is dbus-run-session installed?)");
        inst.child = Some(child);
        // Stagger: let the primary claim its endpoint first.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Reap both with one shared deadline. A script failure exits non-zero
    // (see screenshot_script.rs::run) and fails this step loudly.
    let deadline = Instant::now() + Duration::from_secs(8 * 60);
    for inst in &mut world.instances {
        let child = inst.child.as_mut().expect("spawned above");
        loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => {
                    assert!(status.success(), "instance exited with {status}");
                    break;
                }
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    panic!("instance did not finish within the deadline");
                }
                None => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }
}

/// PNG pixel size straight from the IHDR chunk — no image crate needed.
fn png_size(path: &PathBuf) -> (u32, u32) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(bytes.len() > 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "{} is not a PNG", path.display());
    let be = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap());
    (be(16), be(20))
}

fn assert_fresh_shot(world: &ShotWorld, name: &str) {
    let path = world.out_dir.join(format!("{name}.png"));
    assert!(path.exists(), "{} was not produced", path.display());
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).expect("mtime");
    assert!(mtime >= world.started.expect("session ran"),
        "{} is stale — predates this run", path.display());
    let (w, h) = png_size(&path);
    // 800x600 logical window captured at the script's 2x SCREENSHOT_SCALE.
    assert_eq!((w, h), (1600, 1200), "{} has unexpected size {w}x{h}", path.display());
}

#[then(expr = "a fresh chart screenshot exists")]
async fn chart_exists(world: &mut ShotWorld) { assert_fresh_shot(world, "chart"); }

#[then(expr = "a fresh network screenshot exists")]
async fn network_exists(world: &mut ShotWorld) { assert_fresh_shot(world, "network"); }

#[then(expr = "a fresh synastry screenshot exists")]
async fn synastry_exists(world: &mut ShotWorld) { assert_fresh_shot(world, "synastry"); }

#[then(expr = "a fresh circles screenshot exists")]
async fn circles_exists(world: &mut ShotWorld) { assert_fresh_shot(world, "circles"); }

#[tokio::main]
async fn main() {
    if std::env::var("ZODIA_SCREENSHOTS_OUT").is_err() {
        eprintln!(
            "screenshots: skipped (set ZODIA_SCREENSHOTS_OUT or run \
             scripts/generate-screenshots.sh — needs a display + network)"
        );
        return;
    }
    ShotWorld::run("tests/features").await;
}
