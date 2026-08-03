//! Aspect + interpretation view.
//!
//! `AspectView` is a `SimpleComponent` that renders interpretation rows (via
//! the `InterpRow` factory) and pushes a multi-tab detail page on row
//! activation.  It owns its own `adw::NavigationView`.
//!
//! The detail page presents one `adw::ViewStack` page per key on the row plus
//! a synthetic "Combined" page when there are 2+ keys.  Aspects (single key)
//! get no switcher; placements (sign + house) get [Sign] [House] [Combined].

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::component::{
    AsyncComponent, AsyncComponentParts, AsyncComponentSender, SimpleAsyncComponent,
};
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use zodia_core::{Chart, InterpKey};
use zodia_crypto::IdentityKeypair;
use zodia_store::{ZodiaStore, BaselineStore};

use crate::app::{AppModel, AppMsg};

thread_local! {
    /// Phase F-collab: handle to the doc editor's visible TextBuffer for
    /// the currently-open detail page.  Lets app-level handlers (incoming
    /// remote edits, veto rollbacks) push live body updates into the editor
    /// without forcing the user to nav away + back.  `gtk::TextBuffer` is
    /// `!Send` so a thread_local + main-thread access is the simplest
    /// channel between the relm4 actor + the GTK widget tree.
    pub static ACTIVE_DOC_BUFFER:
        RefCell<Option<(String /* interp_key */, gtk::TextBuffer)>>
        = RefCell::new(None);
}

/// If the currently-open detail page's key matches `interp_key`, set the
/// visible TextBuffer to `new_body`.  No-op otherwise (user is elsewhere).
pub fn refresh_active_doc_body(interp_key: &str, new_body: &str) {
    ACTIVE_DOC_BUFFER.with(|cell| {
        if let Some((k, buf)) = cell.borrow().as_ref() {
            if k == interp_key {
                buf.set_text(new_body);
            }
        }
    });
}
use crate::aspect_list::{AspectItem, KeyEntry};
use crate::interp_row::{InterpRow, InterpRowInit, InterpRowOut};

// ── public entry point ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum AspectViewKind {
    Natal,
    Synastry,
}

pub struct AspectViewInit {
    pub kind:             AspectViewKind,
    pub items:            Vec<AspectItem>,
    /// Placement rows rendered above the aspects group (Natal only).
    pub placements_items: Vec<AspectItem>,
    /// No longer used after PR2; kept on the type for callers' convenience.
    #[allow(dead_code)]
    pub chart:            Option<Rc<Chart>>,
    pub store:            ZodiaStore,
    pub baseline:         Rc<BaselineStore>,
    pub identity:         Rc<IdentityKeypair>,
    pub parent_sender:    AsyncComponentSender<AppModel>,
}

/// Spawn the component and return its root widget; runtime detached so the
/// caller doesn't have to hold a `Controller`.
pub fn launch(init: AspectViewInit) -> adw::NavigationView {
    let mut ctl = <AspectView as AsyncComponent>::builder().launch(init).detach();
    let widget = ctl.widget().clone();
    ctl.detach_runtime();
    widget
}

// ── component ─────────────────────────────────────────────────────────────────

pub struct AspectView {
    nav:              adw::NavigationView,
    store:            ZodiaStore,
    baseline:         Rc<BaselineStore>,
    identity:         Rc<IdentityKeypair>,
    parent_sender:    AsyncComponentSender<AppModel>,
    #[allow(dead_code)]
    rows:             FactoryVecDeque<InterpRow>,
    #[allow(dead_code)]
    placements_rows:  Option<FactoryVecDeque<InterpRow>>,
}

#[derive(Debug)]
pub enum AspectViewMsg {
    OpenDetail {
        keys:            Vec<KeyEntry>,
        transit_context: Option<String>,
    },
}

#[relm4::component(async, pub)]
impl SimpleAsyncComponent for AspectView {
    type Init   = AspectViewInit;
    type Input  = AspectViewMsg;
    type Output = ();

    view! {
        #[root]
        adw::NavigationView {
            set_vexpand: true,

            adw::NavigationPage {
                set_title: "Aspects",
                #[wrap(Some)]
                set_child = &gtk::ScrolledWindow {
                    set_hscrollbar_policy: gtk::PolicyType::Never,
                    set_vexpand: true,
                    #[wrap(Some)]
                    set_child = &adw::Clamp {
                        set_maximum_size: 720,
                        set_margin_top: 8,
                        set_margin_bottom: 8,
                        set_margin_start: 12,
                        set_margin_end: 12,

                        #[wrap(Some)]
                        #[name(content_box)]
                        set_child = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 16,

                            #[local_ref]
                            interp_group -> adw::PreferencesGroup {},

                            #[name(empty_label)]
                            gtk::Label {
                                set_label: "No aspects within default orbs",
                                add_css_class: "dim-label",
                                set_halign: gtk::Align::Center,
                                set_margin_top: 12,
                                set_visible: false,
                            },
                        },
                    },
                },
            },
        }
    }

    async fn init(
        init: Self::Init,
        root: Self::Root,
        sender: AsyncComponentSender<Self>,
    ) -> AsyncComponentParts<Self> {
        // Aspects factory.
        let mut row_inits: Vec<InterpRowInit> = Vec::with_capacity(init.items.len());
        for it in &init.items {
            row_inits.push(build_row_init(it, &init.store, &init.baseline).await);
        }
        let items_empty = row_inits.is_empty();

        let mut rows: FactoryVecDeque<InterpRow> = FactoryVecDeque::builder()
            .launch(adw::PreferencesGroup::new())
            .forward(sender.input_sender(), |out| match out {
                InterpRowOut::Activate { keys, transit_context } =>
                    AspectViewMsg::OpenDetail { keys, transit_context },
            });
        {
            let mut g = rows.guard();
            for ri in row_inits { g.push_back(ri); }
        }

        let interp_group = rows.widget().clone();
        interp_group.set_title("Aspects");

        // Placements factory (Natal only).
        let placements_rows: Option<FactoryVecDeque<InterpRow>> =
            if matches!(init.kind, AspectViewKind::Natal) && !init.placements_items.is_empty() {
                let mut p_inits: Vec<InterpRowInit> =
                    Vec::with_capacity(init.placements_items.len());
                for it in &init.placements_items {
                    p_inits.push(build_row_init(it, &init.store, &init.baseline).await);
                }
                let mut p_rows: FactoryVecDeque<InterpRow> = FactoryVecDeque::builder()
                    .launch(adw::PreferencesGroup::new())
                    .forward(sender.input_sender(), |out| match out {
                        InterpRowOut::Activate { keys, transit_context } =>
                            AspectViewMsg::OpenDetail { keys, transit_context },
                    });
                {
                    let mut g = p_rows.guard();
                    for ri in p_inits { g.push_back(ri); }
                }
                p_rows.widget().set_title("Placements");
                Some(p_rows)
            } else { None };

        let model = AspectView {
            nav: root.clone(),
            store:         init.store,
            baseline:      init.baseline,
            identity:      init.identity,
            parent_sender: init.parent_sender,
            rows,
            placements_rows,
        };

        let widgets = view_output!();
        widgets.empty_label.set_visible(items_empty);

        if let Some(p_rows) = model.placements_rows.as_ref() {
            widgets.content_box.prepend(p_rows.widget());
        }

        AsyncComponentParts { model, widgets }
    }

    async fn update(&mut self, msg: Self::Input, _sender: AsyncComponentSender<Self>) {
        match msg {
            AspectViewMsg::OpenDetail { keys, transit_context } => {
                let page = detail_page(
                    &keys,
                    transit_context,
                    &self.store,
                    &self.baseline,
                    Rc::clone(&self.identity),
                    self.parent_sender.clone(),
                ).await;
                self.nav.push(&page);
            }
        }
    }
}

// ── detail page ───────────────────────────────────────────────────────────────

/// Build the detail page for a row's keys.
///
/// Layout (uniform across 1-key and multi-key rows):
///   1. Optional Timing group (transit aspects)
///   2. One Interpretations group per `KeyEntry` — labelled with the entry's
///      label.  Stacked top-to-bottom; this is the always-visible "Combined"
///      reading.  Affirm buttons are interactive.
///   3. Contribute group with a single text entry, a radio row picking which
///      key the contribution targets (hidden when `keys.len() == 1`), and a
///      Share button.
pub async fn detail_page(
    keys: &[KeyEntry],
    transit_context: Option<String>,
    store: &ZodiaStore,
    baseline: &BaselineStore,
    identity: Rc<IdentityKeypair>,
    sender: AsyncComponentSender<AppModel>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    let header  = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let page_title = keys.first()
        .map(|e| e.key.plain_name())
        .unwrap_or_default();
    let title_lbl = gtk::Label::new(Some(&page_title));
    title_lbl.add_css_class("title");
    header.set_title_widget(Some(&title_lbl));

    toolbar.add_top_bar(&header);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_vexpand(true);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(640);
    clamp.set_margin_top(16);
    clamp.set_margin_bottom(24);
    clamp.set_margin_start(16);
    clamp.set_margin_end(16);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);

    // ── timing (transits) ─────────────────────────────────────────────────────
    if let Some(ctx) = transit_context {
        let timing_group = adw::PreferencesGroup::new();
        timing_group.set_title("Timing");
        let timing_row = adw::ActionRow::new();
        timing_row.set_title(&ctx);
        timing_group.add(&timing_row);
        content.append(&timing_group);
    }

    // ── Phase F-collab: community reading + inline editor + revisions ────────
    for entry in keys {
        let doc_group = build_doc_reading_group(
            &entry.key,
            store,
            baseline,
            Rc::clone(&identity),
            sender.clone(),
        ).await;
        if keys.len() > 1 {
            doc_group.set_title(&format!("{} — Community reading", entry.label));
        }
        content.append(&doc_group);
    }
    let _ = identity;

    clamp.set_child(Some(&content));
    scroll.set_child(Some(&clamp));
    toolbar.set_content(Some(&scroll));

    let page = adw::NavigationPage::new(&toolbar, &page_title);

    // Presence-departure: fire a leave heartbeat for every key on this page
    // when the NavigationPage hides (back-button or push-over).  Pairs with
    // the join emitted by `build_doc_reading_group` on construction.
    let leave_keys: Vec<String> = keys.iter().map(|e| e.key.to_sig()).collect();
    let leave_sender = sender.clone();
    page.connect_hiding(move |_| {
        for k in &leave_keys {
            leave_sender.input(AppMsg::EditorPresence {
                interp_key: k.clone(),
                joined:     false,
            });
        }
        // Detach the live-refresh handle so subsequent doc events don't
        // poke into a no-longer-visible TextBuffer.
        ACTIVE_DOC_BUFFER.with(|cell| { *cell.borrow_mut() = None; });
    });

    page
}

/// Build the inline community-reading + editor for `key`.  The page is
/// the editor — no separate "view" + "edit" modes, no modal dialog.
/// TextView holds the current doc body; Publish button below commits an
/// edit op; a Revisions section surfaces the current revision hash and
/// recent-editor attribution.
async fn build_doc_reading_group(
    key:      &InterpKey,
    store:    &ZodiaStore,
    baseline: &BaselineStore,
    identity: Rc<IdentityKeypair>,
    sender:   AsyncComponentSender<AppModel>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Community reading");
    group.set_description(Some(
        "Local-first document the whole community refines together.  \
         Recent editors may veto changes within 7 days.",
    ));

    let key_sig = key.to_sig();
    let me_bytes: [u8; 32] = identity.public_key();

    // Resolve current body + revision from persisted Loro doc, else
    // bundled baseline, else empty.
    let me_vk = zodia_doc::VerifyingKey::from_bytes(&me_bytes).ok();
    let (display_body, current_rev) = match (&me_vk, store.doc_load(&key_sig).await) {
        (Some(vk), Ok(Some(bytes))) => {
            match zodia_doc::InterpDoc::from_snapshot(vk, &bytes) {
                Ok(d) => (d.body_text(), Some(d.current_rev())),
                Err(_) => (baseline.lookup(key).unwrap_or("").to_string(), None),
            }
        }
        _ => (baseline.lookup(key).unwrap_or("").to_string(), None),
    };

    // Announce presence on page open.  Departure heartbeat lives on the
    // NavigationView's pop callback in a follow-up — for now we only fire
    // join; remote peers' presence eventually times out client-side.
    sender.input(AppMsg::EditorPresence {
        interp_key: key_sig.clone(),
        joined:     true,
    });

    // Lazy subscription (Phase C-2): no-ops for keys already permanently
    // subscribed via the user's own chart; opens/extends a grace-period
    // subscription otherwise. See docs/prd/granular-topic-subscription.md.
    sender.input(AppMsg::TouchKeySubscription { interp_key: key_sig.clone() });

    // ── editor row: TextView + Publish + presence indicator ──────────────────
    let edit_row = adw::ActionRow::new();
    edit_row.set_activatable(false);
    edit_row.set_title("Reading");
    edit_row.set_subtitle("");

    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&display_body);
    // Register this buffer as the active doc target so app-level handlers
    // (DocEdited/Veto/Rollback) can refresh the visible body without a
    // re-nav.  Cleared by the NavigationPage's `connect_hiding` below.
    ACTIVE_DOC_BUFFER.with(|cell| {
        *cell.borrow_mut() = Some((key_sig.clone(), buffer.clone()));
    });
    let text_view = gtk::TextView::with_buffer(&buffer);
    text_view.set_wrap_mode(gtk::WrapMode::WordChar);
    text_view.set_top_margin(8);
    text_view.set_bottom_margin(8);
    text_view.set_left_margin(8);
    text_view.set_right_margin(8);
    text_view.set_accepts_tab(false);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_min_content_height(180);
    scroll.set_max_content_height(320);
    scroll.set_hexpand(true);
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_child(Some(&text_view));

    let editor_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    editor_box.set_margin_top(8);
    editor_box.set_margin_bottom(8);
    editor_box.set_margin_start(12);
    editor_box.set_margin_end(12);
    editor_box.append(&scroll);

    // Publish button row.
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);

    let publish_btn = gtk::Button::with_label("Publish edit");
    publish_btn.add_css_class("suggested-action");
    let buf_for_btn = buffer.clone();
    let key_for_btn = key_sig.clone();
    let sender_for_btn = sender.clone();
    publish_btn.connect_clicked(move |_| {
        let (s, e) = buf_for_btn.bounds();
        let text = buf_for_btn.text(&s, &e, false).to_string();
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            sender_for_btn.input(AppMsg::PublishDocEdit {
                interp_key: key_for_btn.clone(),
                new_body:   trimmed,
            });
        }
    });

    // Named to avoid colliding with the unrelated "Circles" privacy feature
    // (private encrypted sharing, see zodia-circles) — this is a live voice
    // room, nothing to do with that. Was "Start discussion" / "voice
    // circle" in copy, which read as the same feature and confused at
    // least one real user into expecting circle-creation here.
    let audio_btn = gtk::Button::with_label("Talk about this");
    audio_btn.add_css_class("pill");
    audio_btn.set_tooltip_text(Some(
        "Open a live voice room on this reading.  Anyone editing the doc \
         can join from their sidebar.  Audio mesh, max 6 participants.",
    ));
    let key_for_audio = key_sig.clone();
    let sender_for_audio = sender.clone();
    audio_btn.connect_clicked(move |_| {
        sender_for_audio.input(AppMsg::StartEditorAudio {
            interp_key: key_for_audio.clone(),
        });
    });

    // Share-to-circle, directly reachable from the content itself instead
    // of only via a small icon on a Sky feed card once it happens to
    // appear there — circles previously had no real "front door".
    let share_btn = gtk::Button::from_icon_name("avatar-default-symbolic");
    share_btn.add_css_class("flat");
    share_btn.add_css_class("circular");
    share_btn.set_tooltip_text(Some("Share to a circle"));
    let key_for_share = key_sig.clone();
    let sender_for_share = sender.clone();
    share_btn.connect_clicked(move |_| {
        sender_for_share.input(AppMsg::OpenShareToCirclePicker {
            interp_key: key_for_share.clone(),
            body:       String::new(),
        });
    });

    actions.append(&share_btn);
    actions.append(&audio_btn);
    actions.append(&publish_btn);
    editor_box.append(&actions);

    // Offline-first hint.
    let hint = gtk::Label::new(Some(
        "Saved locally first, syncs to peers when you're connected.",
    ));
    hint.set_halign(gtk::Align::Start);
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");
    editor_box.append(&hint);

    let body_holder = adw::PreferencesRow::new();
    body_holder.set_child(Some(&editor_box));
    group.add(&body_holder);
    let _ = edit_row;

    // ── revisions section ────────────────────────────────────────────────────
    let ring = store.block_ring_get(&key_sig, &zodia_doc::BODY_BLOCK_ID).await
        .unwrap_or_default();

    let rev_group = adw::PreferencesGroup::new();
    rev_group.set_title("Revisions");
    rev_group.set_description(Some(
        "Recent edits.  Expand 'Previous version' to compare; use Veto to roll back.",
    ));

    if let Some(rev) = current_rev {
        let current_row = adw::ActionRow::new();
        current_row.set_title("Current revision");
        current_row.set_subtitle(&hex::encode(&rev[..8]));
        // Affirm button — ♡ this revision so the community ranking signal
        // reflects current-doc taste, not "the row that won years ago."
        let affirm_btn = gtk::Button::from_icon_name("emblem-favorite-symbolic");
        affirm_btn.add_css_class("flat");
        affirm_btn.add_css_class("circular");
        affirm_btn.set_valign(gtk::Align::Center);
        affirm_btn.set_tooltip_text(Some(
            "Affirm this revision — your ♡ attaches to the current text, \
             not to a frozen interpretation row.",
        ));
        let key_for_aff = key_sig.clone();
        let sender_for_aff = sender.clone();
        let rev_copy = rev;
        affirm_btn.connect_clicked(move |b| {
            sender_for_aff.input(AppMsg::AffirmDocRev {
                interp_key: key_for_aff.clone(),
                target_rev: rev_copy,
            });
            // Visual feedback: dim the button after click so the user
            // sees their tap registered (idempotent on the server side).
            b.set_sensitive(false);
        });
        current_row.add_suffix(&affirm_btn);
        rev_group.add(&current_row);
    } else {
        let stub_row = adw::ActionRow::new();
        stub_row.set_title("No edits yet");
        stub_row.set_subtitle("Publish the first edit to start a revision history.");
        rev_group.add(&stub_row);
    }

    // Previous-version body (the snapshot we'd restore on veto).
    if let Ok(Some(meta)) = store.doc_load_meta(&key_sig).await {
        if let (Some(prior_bytes), Some(vk)) = (&meta.prior_snapshot, me_vk.as_ref()) {
            if let Ok(prior_doc) = zodia_doc::InterpDoc::from_snapshot(vk, prior_bytes) {
                let prev_body = prior_doc.body_text();
                let author_tag = meta.last_edit_author
                    .map(|a| format!("···{}", hex::encode_upper(&a[..4])))
                    .unwrap_or_else(|| "unknown".into());
                let when = meta.last_edit_ts
                    .map(relative_age)
                    .unwrap_or_else(|| "—".into());
                let expander = adw::ExpanderRow::new();
                expander.set_title("Previous version");
                expander.set_subtitle(&format!("before {} edited · {}", author_tag, when));
                let buf = gtk::TextBuffer::new(None);
                buf.set_text(&prev_body);
                let tv = gtk::TextView::with_buffer(&buf);
                tv.set_editable(false);
                tv.set_cursor_visible(false);
                tv.set_wrap_mode(gtk::WrapMode::WordChar);
                tv.set_top_margin(8);
                tv.set_bottom_margin(8);
                tv.set_left_margin(8);
                tv.set_right_margin(8);
                let scroll = gtk::ScrolledWindow::new();
                scroll.set_min_content_height(120);
                scroll.set_max_content_height(280);
                scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
                scroll.set_child(Some(&tv));
                let holder = adw::PreferencesRow::new();
                holder.set_child(Some(&scroll));
                expander.add_row(&holder);
                rev_group.add(&expander);
            }
        }
    }

    if ring.is_empty() {
        let r = adw::ActionRow::new();
        r.set_title("Recent editors");
        r.set_subtitle("None yet — be the first.");
        rev_group.add(&r);
    } else {
        // Local user is "in the ring" if their pubkey appears as one of the
        // last RING_SIZE editors.  Veto authority targets only the newest
        // entry and only within the 7-day window.
        let me_in_ring = ring.iter().any(|(pk, _, _)| pk == &me_bytes);
        let newest_idx = ring.len() - 1;  // ring is FIFO oldest→newest
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let window_secs = zodia_doc::VETO_WINDOW_DAYS * 86_400;
        for (i, (pk, op_id, ts)) in ring.iter().enumerate().rev() {
            let r = adw::ActionRow::new();
            r.set_title(&format!("···{}", hex::encode_upper(&pk[..4])));
            let mine_marker = if pk == &me_bytes { "  (you)" } else { "" };
            r.set_subtitle(&format!(
                "edit {}  ·  {}{}",
                hex::encode(&op_id[..4]),
                relative_age(*ts),
                mine_marker,
            ));
            let is_newest = i == newest_idx;
            let in_window = now.saturating_sub(*ts) <= window_secs;
            let not_self = pk != &me_bytes;
            if is_newest && me_in_ring && in_window && not_self {
                let veto_btn = gtk::Button::with_label("Veto");
                veto_btn.add_css_class("destructive-action");
                veto_btn.set_valign(gtk::Align::Center);
                veto_btn.set_tooltip_text(Some(
                    "Roll back this edit.  Only newest edits within 7 days, \
                     and only if you authored a recent edit on this doc.",
                ));
                let key_for_veto = key_sig.clone();
                let op_for_veto = *op_id;
                let sender_for_veto = sender.clone();
                veto_btn.connect_clicked(move |_| {
                    sender_for_veto.input(AppMsg::ProposeDocVeto {
                        interp_key:        key_for_veto.clone(),
                        target_edit_op_id: op_for_veto,
                    });
                });
                r.add_suffix(&veto_btn);
            }
            rev_group.add(&r);
        }
    }

    // Attach revisions to the same container via a wrapper widget.
    // PreferencesGroup doesn't allow direct nesting, so we expose `group`
    // alone and append `rev_group` separately from the caller.  Cleaner
    // approach below: caller appends two groups; for now we hack by
    // setting rev_group as a header_suffix.
    group.set_header_suffix(Some(&rev_group_into_button(rev_group, &key_sig)));

    group
}

/// Wrap the revisions PreferencesGroup in a MenuButton popover so it
/// surfaces as a discreet "History" affordance in the header without
/// claiming whole-page real estate.  Clicking the icon reveals the
/// revisions list in a popover.
fn rev_group_into_button(
    rev_group: adw::PreferencesGroup,
    _key_sig:  &str,
) -> gtk::MenuButton {
    let btn = gtk::MenuButton::new();
    btn.set_icon_name("document-open-recent-symbolic");
    btn.set_tooltip_text(Some("Revisions"));
    btn.add_css_class("flat");

    let popover = gtk::Popover::new();
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_min_content_width(320);
    scroll.set_min_content_height(220);
    scroll.set_max_content_height(420);
    scroll.set_child(Some(&rev_group));
    popover.set_child(Some(&scroll));
    btn.set_popover(Some(&popover));
    btn
}

fn relative_age(unix: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(unix);
    let d = now.saturating_sub(unix);
    if d < 60         { "now".into() }
    else if d < 3600  { format!("{}m ago", d / 60) }
    else if d < 86400 { format!("{}h ago", d / 3600) }
    else              { format!("{}d ago", d / 86400) }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Best interpretation text for `key`: community DB result first, baseline fallback.
async fn resolve_top_body(store: &ZodiaStore, baseline: &BaselineStore, key: &InterpKey) -> String {
    if let Ok(Some(body)) = store.top_body(key).await {
        return body;
    }
    baseline.lookup(key).map(str::to_owned).unwrap_or_default()
}

/// Convert an `AspectItem` into the factory's `InterpRowInit`.  Body preview is
/// the first non-empty body across the row's keys (so a placement row shows
/// whichever of sign/house has content).
async fn build_row_init(
    it: &AspectItem,
    store: &ZodiaStore,
    baseline: &BaselineStore,
) -> InterpRowInit {
    let mut body_preview = String::new();
    for e in &it.keys {
        let candidate = resolve_top_body(store, baseline, &e.key).await;
        if !candidate.is_empty() {
            body_preview = candidate;
            break;
        }
    }
    InterpRowInit {
        keys:            it.keys.clone(),
        title:           it.title.clone(),
        symbol_line:     Some(it.symbol_line.clone()),
        meta_line:       it.meta_line.clone(),
        transit_context: it.transit_context.clone(),
        body_preview,
    }
}
