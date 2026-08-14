//! Scripted screenshot mode — drives the app through its own message
//! layer instead of synthetic mouse input, so release screenshots can be
//! regenerated deterministically (see `app/tests/screenshots.rs` and
//! `scripts/generate-screenshots.sh`).
//!
//! Activated only when `ZODIA_SCREENSHOT_SCRIPT` points at a script file;
//! completely inert otherwise. The script is line-based, `#` comments and
//! blank lines skipped:
//!
//! ```text
//! set-window-size 1600 1240
//! wait 2
//! wait-state discovered 60      # a peer's announce has been seen
//! connect-first-peer            # -> AppMsg::Reconnect
//! wait-state connected 120      # Tier-1 consent completed
//! goto network
//! wait 1
//! snapshot /tmp/network.png
//! quit
//! ```
//!
//! Two kinds of command, matching where the work has to happen:
//!   - model-level (queries, Reconnect, AcceptConsent, circle ops) are
//!     plain `AppMsg`s sent from the driver thread;
//!   - widget-level (`goto`, `scroll`, `snapshot`, `set-window-size`) ride
//!     an `AppMsg::ScriptUi(..)` that `update()` stashes and `update_view`
//!     executes, since only `update_view` can reach the widget tree.
//!
//! The driver runs on a plain std thread: every step is either a sleep or
//! a blocking round-trip through the relm4 input channel, and the replies
//! come back over a std `mpsc` — no async needed on this side.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::app::AppMsg;
use zodia_net::PeerId;

// ── driver → app messages ─────────────────────────────────────────────────────

/// Widget-level commands, executed by `update_view` (the only place with
/// access to the widget tree).
#[derive(Debug, Clone)]
pub enum ScriptUiCmd {
    /// `content_stack.set_visible_child_name(..)` — "chart" / "sky" / "network".
    Goto(String),
    /// Scroll the first `GtkScrolledWindow` found under the visible
    /// content-stack child by this many pixels (negative scrolls up).
    ScrollBy(f64),
    /// Resize the toplevel window.
    SetWindowSize(i32, i32),
    /// Render the toplevel window's widget tree to a PNG.
    Snapshot(PathBuf),
    /// Switch the visible peer page's `AdwViewStack` to a named page —
    /// "their" / "synastry" / "messages" (see `stargazer_page.rs`).
    PeerTab(String),
    /// Dump every label's text found under the visible content-stack
    /// child's first `GtkListBox` to `path`, one per line — used to assert
    /// on rendered card content (e.g. the Sky feed) without a screenshot.
    DumpSkyText(PathBuf),
}

/// One coherent view of the model state the script can wait on — filled by
/// `update()` (which can also await the SDK client for circle membership).
#[derive(Debug, Clone, Default)]
pub struct ScriptStateSnapshot {
    pub discovered_peer:     Option<PeerId>,
    /// A peer we've already proposed consent to (OutgoingPending) — used
    /// by `connect-first-peer` to retry via `Reconnect` if the first
    /// dial timed out.
    pub pending_peer:        Option<PeerId>,
    pub connected_peer:      Option<PeerId>,
    pub incoming_consent:    bool,
    pub first_circle_id_hex: Option<String>,
    /// Member count of `first_circle_id_hex` (0 when there is no circle).
    pub circle_member_count: usize,
    /// Canonical key signature of whichever transit/sky-aspect/house-transit
    /// is first in the currently-active set, if any — the same key space
    /// `AppMsg::PublishDocEdit` and `transit_ticker::active_item_for_key`
    /// both key off. `None` before the chart exists or nothing is in orb.
    pub first_sky_interp_key: Option<String>,
}

/// Reply channel wrapper so `AppMsg` keeps deriving `Debug`.
pub struct ScriptQueryTx(pub mpsc::Sender<ScriptStateSnapshot>);

impl std::fmt::Debug for ScriptQueryTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptQueryTx")
    }
}

// ── driver ────────────────────────────────────────────────────────────────────

/// Start the driver thread if `ZODIA_SCREENSHOT_SCRIPT` is set. Called once
/// from `AppModel::init`; a no-op in normal runs.
pub fn maybe_start(sender: relm4::Sender<AppMsg>) {
    let Ok(path) = std::env::var("ZODIA_SCREENSHOT_SCRIPT") else { return };
    let script = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("screenshot script {path}: {e}");
            return;
        }
    };
    tracing::info!("screenshot-script mode: driving from {path}");
    std::thread::spawn(move || {
        run(&script, &sender);
    });
}

fn query(sender: &relm4::Sender<AppMsg>) -> ScriptStateSnapshot {
    let (tx, rx) = mpsc::channel();
    let _ = sender.send(AppMsg::ScriptQuery(ScriptQueryTx(tx)));
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default()
}

/// Poll `pred` against fresh state snapshots until it holds or `timeout`
/// elapses. Returns whether it held.
fn wait_state(
    sender: &relm4::Sender<AppMsg>,
    timeout: Duration,
    pred: impl Fn(&ScriptStateSnapshot) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if pred(&query(sender)) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn run(script: &str, sender: &relm4::Sender<AppMsg>) {
    for (lineno, raw) in script.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap();
        let args: Vec<&str> = parts.collect();
        tracing::info!("script[{}]: {line}", lineno + 1);
        let ok = step(cmd, &args, sender);
        if !ok {
            tracing::error!("script[{}] FAILED: {line} — aborting", lineno + 1);
            // A partial screenshot set is worse than none: make the failure
            // loud (non-zero exit) so the harness never mistakes it for done.
            std::process::exit(1);
        }
    }
}

fn step(cmd: &str, args: &[&str], sender: &relm4::Sender<AppMsg>) -> bool {
    match (cmd, args) {
        ("wait", [secs]) => {
            let Ok(s) = secs.parse::<f64>() else { return false };
            std::thread::sleep(Duration::from_secs_f64(s));
            true
        }
        ("set-window-size", [w, h]) => {
            let (Ok(w), Ok(h)) = (w.parse(), h.parse()) else { return false };
            sender.send(AppMsg::ScriptUi(ScriptUiCmd::SetWindowSize(w, h))).is_ok()
        }
        ("goto", [page]) => {
            sender.send(AppMsg::ScriptUi(ScriptUiCmd::Goto(page.to_string()))).is_ok()
        }
        ("scroll", [px]) => {
            let Ok(px) = px.parse::<f64>() else { return false };
            sender.send(AppMsg::ScriptUi(ScriptUiCmd::ScrollBy(px))).is_ok()
        }
        ("snapshot", [path]) => {
            sender.send(AppMsg::ScriptUi(ScriptUiCmd::Snapshot(PathBuf::from(path)))).is_ok()
        }
        ("peer-tab", [name]) => {
            sender.send(AppMsg::ScriptUi(ScriptUiCmd::PeerTab(name.to_string()))).is_ok()
        }
        ("dump-sky-text", [path]) => {
            sender.send(AppMsg::ScriptUi(ScriptUiCmd::DumpSkyText(PathBuf::from(path)))).is_ok()
        }
        ("publish-first-sky-reading", rest) if !rest.is_empty() => {
            match query(sender).first_sky_interp_key {
                Some(interp_key) => sender.send(AppMsg::PublishDocEdit {
                    interp_key, new_body: rest.join(" "), ai_generated: false,
                }).is_ok(),
                None => false,
            }
        }
        ("wait-state", [what, timeout]) => {
            let Ok(t) = timeout.parse::<u64>() else { return false };
            let t = Duration::from_secs(t);
            match *what {
                "discovered" => wait_state(sender, t, |s| s.discovered_peer.is_some()
                    || s.connected_peer.is_some()),
                "connected"        => wait_state(sender, t, |s| s.connected_peer.is_some()),
                "incoming-consent" => wait_state(sender, t, |s| s.incoming_consent),
                "circle"           => wait_state(sender, t, |s| s.first_circle_id_hex.is_some()),
                // A circle whose membership grew past just its creator —
                // i.e. the invite actually landed.
                "circle-member"    => wait_state(sender, t, |s| s.circle_member_count >= 2),
                "has-sky-transit"  => wait_state(sender, t, |s| s.first_sky_interp_key.is_some()),
                other => {
                    tracing::error!("wait-state: unknown state {other}");
                    false
                }
            }
        }
        ("connect-first-peer", []) => {
            // First call: ProposeConsent (a freshly-Discovered peer must
            // enter OutgoingPending; Reconnect early-returns on Discovered).
            // Repeat calls: Reconnect, which retries an OutgoingPending
            // peer whose earlier dial timed out — iroh needs a few seconds
            // after startup before its addresses actually dial. No-op once
            // connected.
            let s = query(sender);
            if s.connected_peer.is_some() {
                true
            } else if let Some(pid) = s.discovered_peer {
                sender.send(AppMsg::ProposeConsent(pid)).is_ok()
            } else if let Some(pid) = s.pending_peer {
                sender.send(AppMsg::Reconnect(pid)).is_ok()
            } else {
                false
            }
        }
        ("accept-consent", []) => sender.send(AppMsg::AcceptConsent).is_ok(),
        // Circles are born from sharing (see docs/prd/circles.md), so this
        // rides the same message the share dialog's "new circle" path uses.
        ("create-circle", rest) if !rest.is_empty() => {
            sender.send(AppMsg::CreateCircleAndShare {
                name:       rest.join(" "),
                interp_key: "natal:mars_square_uranus".to_string(),
                body:       "Impulses clash with unpredictability.".to_string(),
            }).is_ok()
        }
        ("invite-first-peer", []) => {
            let s = query(sender);
            match (s.first_circle_id_hex, s.connected_peer) {
                (Some(id_hex), Some(peer_id)) => sender.send(AppMsg::InviteToCircle {
                    id_hex, peer_id, can_post: false,
                }).is_ok(),
                _ => false,
            }
        }
        ("open-first-circle", []) => {
            match query(sender).first_circle_id_hex {
                Some(id_hex) => sender.send(AppMsg::OpenCirclePage(id_hex)).is_ok(),
                None => false,
            }
        }
        ("open-first-peer", []) => {
            match query(sender).connected_peer {
                Some(pid) => sender.send(AppMsg::OpenStargazer(pid)).is_ok(),
                None => false,
            }
        }
        ("quit", []) => {
            // Graceful: let GTK tear down the window; exit(0) from a helper
            // thread would race the compositor handshake.
            sender.send(AppMsg::ScriptQuit).is_ok()
        }
        _ => {
            tracing::error!("unknown script command: {cmd} {args:?}");
            false
        }
    }
}

// ── widget-side helpers (called from update_view) ────────────────────────────

/// Depth-first search for the first widget of type `T` under `root`.
fn find_descendant<T: gtk4::glib::object::IsA<gtk4::Widget>>(root: &gtk4::Widget) -> Option<T> {
    use gtk4::prelude::*;
    if let Ok(hit) = root.clone().downcast::<T>() {
        return Some(hit);
    }
    let mut child = root.first_child();
    while let Some(w) = child {
        if let Some(found) = find_descendant::<T>(&w) {
            return Some(found);
        }
        child = w.next_sibling();
    }
    None
}

/// Depth-first collection of every non-empty `GtkLabel`'s text under
/// `root`, in tree order. Deliberately doesn't hardcode which child index
/// is a "title" vs a "subtitle" label — layout changes to whatever's being
/// dumped (e.g. `FeedCardWidgets`) don't need this helper to change too.
fn collect_label_texts(root: &gtk4::Widget, out: &mut Vec<String>) {
    use gtk4::prelude::*;
    if let Some(label) = root.downcast_ref::<gtk4::Label>() {
        let text = label.text();
        if !text.is_empty() {
            out.push(text.to_string());
        }
    }
    let mut child = root.first_child();
    while let Some(w) = child {
        collect_label_texts(&w, out);
        child = w.next_sibling();
    }
}

/// Screenshots render at this multiple of the window's logical size — a
/// real "@2x" capture, the same idea as a HiDPI/Retina screenshot: the
/// window itself stays a normal, modest logical size (so the UI looks like
/// an actual app window, not stretched out to fill a huge canvas), while
/// the exported PNG still has enough pixel density to read crisply in
/// docs/store listings.
///
/// Known flake, not fully root-caused: in repeated local runs, `snapshot`
/// occasionally hits "window never became renderable" (`try_snapshot`'s own
/// retry-exhaustion error below) even with a generous retry budget — the
/// same script succeeds cleanly most runs, so it reads as environmental
/// (two real GTK+P2P processes, repeated back-to-back runs) rather than a
/// deterministic bug in this scaling. If it recurs consistently for you,
/// the next thing to check is whether it's specific to `SCREENSHOT_SCALE`
/// > 1.0 (temporarily set it to `1.0` and compare).
const SCREENSHOT_SCALE: f32 = 2.0;

/// Render the window's widget tree to `path` at `SCREENSHOT_SCALE`,
/// retrying on the next frames if the render node comes back empty — which
/// happens when the snapshot lands on the same frame a lazily-built page
/// (e.g. the chart tab on its first `goto`) is still being allocated.
fn try_snapshot(window: gtk4::Window, path: PathBuf, attempts_left: u32) {
    use gtk4::prelude::*;
    let w = window.width() as f64;
    let h = window.height() as f64;
    let paintable = gtk4::WidgetPaintable::new(Some(&window));
    let snapshot = gtk4::Snapshot::new();
    // Scale the recorded content before anything is drawn into it, so
    // every subsequent append (the whole widget tree, via
    // `paintable.snapshot`) lands at `SCREENSHOT_SCALE`x density — the
    // viewport passed to `render_texture` below just has to match.
    snapshot.scale(SCREENSHOT_SCALE, SCREENSHOT_SCALE);
    paintable.snapshot(&snapshot, w, h);
    let node = snapshot.to_node();
    let renderer = window.renderer();
    match (node, renderer) {
        (Some(node), Some(renderer)) => {
            let texture = renderer.render_texture(
                &node,
                Some(&gtk4::graphene::Rect::new(
                    0.0, 0.0,
                    w as f32 * SCREENSHOT_SCALE, h as f32 * SCREENSHOT_SCALE,
                )),
            );
            match texture.save_to_png(&path) {
                Ok(()) => tracing::info!("snapshot saved: {}", path.display()),
                Err(e) => tracing::error!("snapshot {}: {e}", path.display()),
            }
        }
        _ if attempts_left > 0 => {
            tracing::debug!("snapshot: window not renderable yet, retrying \
                             ({attempts_left} attempts left)");
            gtk4::glib::timeout_add_local_once(
                Duration::from_millis(100),
                move || try_snapshot(window, path, attempts_left - 1),
            );
        }
        _ => tracing::error!("snapshot {}: window never became renderable", path.display()),
    }
}

/// Execute one widget-level command. `anchor` is any widget inside the main
/// window (the split view) — used to find the toplevel and the visible page.
pub fn run_ui_cmd(cmd: &ScriptUiCmd, anchor: &gtk4::Widget, content_stack: &gtk4::Stack) {
    use gtk4::prelude::*;
    match cmd {
        ScriptUiCmd::Goto(page) => {
            content_stack.set_visible_child_name(page);
        }
        ScriptUiCmd::ScrollBy(px) => {
            let target = content_stack.visible_child()
                .and_then(|c| find_descendant::<gtk4::ScrolledWindow>(&c));
            match target {
                Some(sw) => {
                    let adj = sw.vadjustment();
                    adj.set_value(adj.value() + px);
                }
                None => tracing::warn!("scroll: no ScrolledWindow under visible page"),
            }
        }
        ScriptUiCmd::PeerTab(name) => {
            let stack = content_stack.visible_child()
                .and_then(|c| find_descendant::<libadwaita::ViewStack>(&c));
            match stack {
                Some(vs) => vs.set_visible_child_name(name),
                None => tracing::warn!("peer-tab: no AdwViewStack under visible page"),
            }
        }
        ScriptUiCmd::DumpSkyText(path) => {
            let target = content_stack.visible_child()
                .and_then(|c| find_descendant::<gtk4::ListBox>(&c));
            let mut lines = Vec::new();
            match &target {
                Some(list) => collect_label_texts(list.upcast_ref(), &mut lines),
                None => tracing::warn!("dump-sky-text: no ListBox under visible page"),
            }
            if let Err(e) = std::fs::write(path, lines.join("\n")) {
                tracing::error!("dump-sky-text {}: {e}", path.display());
            }
        }
        ScriptUiCmd::SetWindowSize(w, h) => {
            if let Some(window) = anchor.root().and_then(|r| r.downcast::<gtk4::Window>().ok()) {
                window.set_default_size(*w, *h);
            }
        }
        ScriptUiCmd::Snapshot(path) => {
            let Some(root) = anchor.root() else {
                tracing::error!("snapshot: no root window");
                return;
            };
            let Ok(window) = root.downcast::<gtk4::Window>() else {
                tracing::error!("snapshot: root is not a window");
                return;
            };
            // 40 attempts * 100ms = 4s budget — up from the original 2s to
            // give a just-navigated page (e.g. straight off
            // `open-first-circle`) more room to finish building before
            // it's renderable. Past this, a much longer budget stopped
            // helping in practice (see this file's `SCREENSHOT_SCALE` doc
            // comment) — "window never became renderable" past ~4s looks
            // like a stuck condition, not a slow one.
            try_snapshot(window, path.clone(), 40);
        }
    }
}
