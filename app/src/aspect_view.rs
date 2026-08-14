//! Aspect + interpretation view.
//!
//! `AspectView` is a `SimpleComponent` that renders interpretation rows (via
//! the `InterpRow` factory) and pushes a multi-tab detail page on row
//! activation.  It owns its own `adw::NavigationView`.
//!
//! The detail page presents one `adw::ViewStack` page per key on the row plus
//! a synthetic "Combined" page when there are 2+ keys.  Aspects (single key)
//! get no switcher; placements (sign + house) get [Sign] [House] [Combined].

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::glib;
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
use crate::util::lon_to_sign_deg;

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
    /// Used to compute the natal Moon phase shown in the Placements
    /// section's description (Natal only) — `None` for Synastry/Transit.
    pub chart:            Option<Rc<Chart>>,
    pub store:            ZodiaStore,
    pub baseline:         Rc<BaselineStore>,
    pub identity:         Rc<IdentityKeypair>,
    pub parent_sender:    AsyncComponentSender<AppModel>,
    pub ollama_models:    Vec<String>,
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
    /// Locally-installed Ollama model names, if Ollama was detected at
    /// app startup — empty means "not available," and no
    /// "Generate with AI" button is offered. See `AppModel`'s own
    /// `ollama_models` field for how this gets populated.
    ollama_models:    Vec<String>,
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
                                add_css_class: "dimmed",
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
                // Natal Moon phase — a real, commonly-discussed astrological
                // data point ("born under a Full Moon") that previously had
                // nowhere to live in the UI at all.
                if let Some(chart) = &init.chart {
                    if let (Some(sun), Some(moon)) = (
                        chart.positions.get(zodia_core::Planet::Sun),
                        chart.positions.get(zodia_core::Planet::Moon),
                    ) {
                        let phase = zodia_core::moon_phase_from_longitudes(sun, moon);
                        p_rows.widget().set_description(Some(&format!(
                            "Born under a {} {}", phase.symbol(), phase.name(),
                        )));
                    }
                }
                Some(p_rows)
            } else { None };

        // Search filter — the natal aspect list can run 20-45 flat rows
        // with no built-in way to narrow it (confirmed against the actual
        // FactoryVecDeque usage here, no prior filtering existed). A
        // GtkSearchEntry filtering by row title is the proportionate fix
        // per the designing-gnome-ui skill's own list-widget guidance —
        // this list runs tens of rows, not the thousands a GtkListView +
        // factory rewrite would actually be justified by.
        let placements_group = placements_rows.as_ref().map(|p| p.widget().clone());
        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search aspects…"));
        search_entry.set_visible(!items_empty);
        {
            let aspects_group   = interp_group.clone();
            let placements_group = placements_group.clone();
            search_entry.connect_search_changed(move |entry| {
                let query = entry.text().to_lowercase();
                filter_preferences_group_rows(&aspects_group, &query);
                if let Some(p) = &placements_group {
                    filter_preferences_group_rows(p, &query);
                }
            });
        }

        let model = AspectView {
            nav: root.clone(),
            store:         init.store,
            baseline:      init.baseline,
            identity:      init.identity,
            parent_sender: init.parent_sender,
            ollama_models: init.ollama_models,
            rows,
            placements_rows,
        };

        let widgets = view_output!();
        widgets.empty_label.set_visible(items_empty);

        if let Some(p_rows) = model.placements_rows.as_ref() {
            widgets.content_box.prepend(p_rows.widget());
        }
        widgets.content_box.prepend(&search_entry);

        // Elemental / modal balance — a classic natal-chart summary
        // ("Fire-dominant", "a lot of Cardinal energy") the app never
        // surfaced despite already deriving each planet's sign. Two plain
        // non-interactive rows; no InterpKey plumbing needed since this is
        // a pure derived summary, not contributable content.
        if let Some(chart) = &init.chart {
            let b = zodia_core::natal_balance(chart);
            let balance_group = adw::PreferencesGroup::new();
            balance_group.set_title("Balance");

            let elements_row = adw::ActionRow::new();
            elements_row.set_title("Elements");
            elements_row.set_subtitle(&format!(
                "Fire {} · Earth {} · Air {} · Water {}", b.fire, b.earth, b.air, b.water,
            ));
            elements_row.set_activatable(false);
            balance_group.add(&elements_row);

            let modalities_row = adw::ActionRow::new();
            modalities_row.set_title("Modalities");
            modalities_row.set_subtitle(&format!(
                "Cardinal {} · Fixed {} · Mutable {}", b.cardinal, b.fixed, b.mutable,
            ));
            modalities_row.set_activatable(false);
            balance_group.add(&modalities_row);

            // Stelliums — 3+ planets clustered in one sign or house, a
            // well-known "concentration of emphasis" pattern with no prior
            // home in the UI at all.
            for (sign, planets) in zodia_core::stelliums_by_sign(&chart.positions) {
                let row = adw::ActionRow::new();
                row.set_title(&format!("Stellium in {}", capitalize(zodia_core::interp::sign_name_lower(sign))));
                row.set_subtitle(&planets.iter().map(|p| p.symbol()).collect::<Vec<_>>().join(" "));
                row.set_activatable(false);
                balance_group.add(&row);
            }
            for (house, planets) in zodia_core::stelliums_by_house(chart) {
                let row = adw::ActionRow::new();
                row.set_title(&format!("Stellium in House {house}"));
                row.set_subtitle(&planets.iter().map(|p| p.symbol()).collect::<Vec<_>>().join(" "));
                row.set_activatable(false);
                balance_group.add(&row);
            }

            // Named aspect patterns — Grand Trine / T-Square — geometric
            // configurations astrologers specifically look for, previously
            // undetected even though every individual aspect they're built
            // from was already computed.
            for pattern in zodia_core::detect_patterns(&chart.natal_aspects()) {
                let row = adw::ActionRow::new();
                match pattern {
                    zodia_core::ChartPattern::GrandTrine(a, b, c) => {
                        row.set_title("Grand Trine");
                        row.set_subtitle(&format!("{} {} {}", a.symbol(), b.symbol(), c.symbol()));
                    }
                    zodia_core::ChartPattern::TSquare { apex, opposition } => {
                        row.set_title("T-Square");
                        row.set_subtitle(&format!(
                            "apex {}  ·  {} ☍ {}",
                            apex.symbol(), opposition.0.symbol(), opposition.1.symbol(),
                        ));
                    }
                }
                row.set_activatable(false);
                balance_group.add(&row);
            }

            widgets.content_box.prepend(&balance_group);

            // House cusps — chart.houses.cusps was already computed for
            // every chart but only ever consulted for a stub-detection
            // check, never actually shown. A full 12-cusp table is a
            // standard chart display this app never had.
            let is_stub = chart.houses.cusps.iter().all(|&c| c == 0.0);
            if !is_stub {
                let houses_group = adw::PreferencesGroup::new();
                houses_group.set_title("Houses");
                for (i, &cusp) in chart.houses.cusps.iter().enumerate() {
                    let (sign, deg_str) = lon_to_sign_deg(cusp);
                    let row = adw::ActionRow::new();
                    row.set_title(&format!("House {}", i + 1));
                    row.set_subtitle(&format!(
                        "{} {deg_str}", capitalize(zodia_core::interp::sign_name_lower(sign)),
                    ));
                    row.set_activatable(false);
                    houses_group.add(&row);
                }
                widgets.content_box.prepend(&houses_group);
            }

            // "Big Three" — Sun/Moon/Ascendant signs together, the single
            // most commonly asked-for at-a-glance summary in modern pop
            // astrology. Every value already existed separately (Sun/Moon
            // from placements, Ascendant from houses) with nowhere
            // combining them into one headline. Placed above everything
            // else via this second prepend.
            if let Some(bt) = chart.big_three() {
                let big_three_label = gtk::Label::new(Some(&format!(
                    "☉ {}  ·  ☽ {}  ·  ASC {}",
                    capitalize(zodia_core::interp::sign_name_lower(bt.sun_sign)),
                    capitalize(zodia_core::interp::sign_name_lower(bt.moon_sign)),
                    capitalize(zodia_core::interp::sign_name_lower(bt.ascendant_sign)),
                )));
                big_three_label.add_css_class("title-4");
                big_three_label.set_halign(gtk::Align::Center);
                big_three_label.set_margin_bottom(4);
                widgets.content_box.prepend(&big_three_label);
            }
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
                    &self.ollama_models,
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
    ollama_models: &[String],
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
            ollama_models,
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
    key:           &InterpKey,
    store:         &ZodiaStore,
    baseline:      &BaselineStore,
    identity:      Rc<IdentityKeypair>,
    sender:        AsyncComponentSender<AppModel>,
    ollama_models: &[String],
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Community reading");

    let key_sig = key.to_sig();

    // Disclosure banner — deliberately part of the group description (the
    // most prominent, always-visible spot) rather than only in the
    // "Recent editors" popover, since a casual reader shouldn't have to
    // dig into history to learn part of what they're reading was
    // AI-drafted. See `zodia_store::doc_has_ai_generated_content`'s doc
    // comment for the "any block, any ring position" scoping.
    let has_ai_content = store.doc_has_ai_generated_content(&key_sig).await.unwrap_or(false);
    group.set_description(Some(if has_ai_content {
        "Local-first document the whole community refines together.  \
         Recent editors may veto changes within 7 days.  \
         🤖 Includes AI-drafted content — see Recent editors for who."
    } else {
        "Local-first document the whole community refines together.  \
         Recent editors may veto changes within 7 days."
    }));
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

    // Set once by the "Generate with AI" button below (if used) and read
    // by Publish at submit time. Deliberately sticky across further manual
    // edits to the same draft — "I used AI assistance for this
    // submission" is the honest disclosure, not "this exact byte range is
    // unedited AI output," which no UI could track meaningfully anyway.
    let ai_generated_flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    let publish_btn = gtk::Button::with_label("Publish edit");
    publish_btn.add_css_class("suggested-action");
    let buf_for_btn = buffer.clone();
    let key_for_btn = key_sig.clone();
    let sender_for_btn = sender.clone();
    let ai_flag_for_publish = Rc::clone(&ai_generated_flag);
    publish_btn.connect_clicked(move |_| {
        let (s, e) = buf_for_btn.bounds();
        let text = buf_for_btn.text(&s, &e, false).to_string();
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            sender_for_btn.input(AppMsg::PublishDocEdit {
                interp_key:   key_for_btn.clone(),
                new_body:     trimmed,
                ai_generated: ai_flag_for_publish.get(),
            });
            ai_flag_for_publish.set(false);
        }
    });

    // "✨ Generate with AI" — only offered when a local Ollama instance was
    // actually detected at startup (see `AppModel::ollama_models`'s doc
    // comment). Drafts into the same buffer Publish reads from; the user
    // can edit before submitting exactly as with a hand-written draft.
    let generate_btn = gtk::Button::with_label("✨ Generate with AI");
    if let Some(model) = ollama_models.first() {
        let buf_for_gen = buffer.clone();
        let model = model.clone();
        let prompt = zodia_core::build_interp_prompt(key);
        let ai_flag_for_gen = Rc::clone(&ai_generated_flag);
        let btn_for_gen = generate_btn.clone();
        generate_btn.connect_clicked(move |_| {
            let buf = buf_for_gen.clone();
            let model = model.clone();
            let prompt = prompt.clone();
            let flag = Rc::clone(&ai_flag_for_gen);
            let btn = btn_for_gen.clone();
            btn.set_sensitive(false);
            btn.set_label("Generating…");
            glib::spawn_future_local(async move {
                match zodia_llm::OllamaClient::new().generate(&model, &prompt).await {
                    Ok(text) => {
                        buf.set_text(text.trim());
                        flag.set(true);
                    }
                    Err(e) => {
                        tracing::warn!("ollama generate failed: {e}");
                    }
                }
                btn.set_sensitive(true);
                btn.set_label("✨ Generate with AI");
            });
        });
    } else {
        generate_btn.set_visible(false);
    }

    // Named to avoid colliding with the unrelated "Circles" privacy feature
    // (private encrypted sharing, see zodia-circles) — this is a live voice
    // room, nothing to do with that. Was "Start discussion" / "voice
    // circle" in copy, which read as the same feature and confused at
    // least one real user into expecting circle-creation here.
    // Plain label, not `pill` — that class reads as a prominent call-to-
    // action (its other use in this app is the empty-state "contribute a
    // reading" button), which this secondary action isn't. Keeping this
    // and `generate_btn` both plain-labeled, `share_btn` icon-only-flat,
    // and `publish_btn` the row's one `suggested-action` gives the row a
    // clear two-tier hierarchy instead of four different button shapes.
    let audio_btn = gtk::Button::with_label("Talk about this");
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
    actions.append(&generate_btn);
    actions.append(&publish_btn);
    editor_box.append(&actions);

    // Offline-first hint.
    let hint = gtk::Label::new(Some(
        "Saved locally first, syncs to peers when you're connected.",
    ));
    hint.set_halign(gtk::Align::Start);
    hint.add_css_class("caption");
    hint.add_css_class("dimmed");
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
        // Affirm control — ♡ this revision so the community ranking signal
        // reflects current-doc taste, not "the row that won years ago."
        // A toggle, not a one-shot button: a misclick (or genuine change
        // of mind) is always self-service, same as leaving a circle.
        let already_affirmed = store.doc_has_affirmed(&key_sig, &rev, &me_bytes).await
            .unwrap_or(false);
        let affirm_btn = gtk::ToggleButton::new();
        affirm_btn.set_icon_name("emblem-favorite-symbolic");
        affirm_btn.add_css_class("flat");
        affirm_btn.add_css_class("circular");
        affirm_btn.set_valign(gtk::Align::Center);
        affirm_btn.set_tooltip_text(Some(
            "Affirm this revision — your ♡ attaches to the current text, \
             not to a frozen interpretation row. Click again to undo.",
        ));
        // Set before connecting the handler so the initial state doesn't
        // itself fire a spurious (re-)affirm/retract publish.
        affirm_btn.set_active(already_affirmed);
        let key_for_aff = key_sig.clone();
        let sender_for_aff = sender.clone();
        let rev_copy = rev;
        affirm_btn.connect_toggled(move |b| {
            if b.is_active() {
                sender_for_aff.input(AppMsg::AffirmDocRev {
                    interp_key: key_for_aff.clone(),
                    target_rev: rev_copy,
                });
            } else {
                sender_for_aff.input(AppMsg::RetractDocAffirm {
                    interp_key: key_for_aff.clone(),
                    target_rev: rev_copy,
                });
            }
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
        let me_in_ring = ring.iter().any(|(pk, _, _, _)| pk == &me_bytes);
        let newest_idx = ring.len() - 1;  // ring is FIFO oldest→newest
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let window_secs = zodia_doc::VETO_WINDOW_DAYS * 86_400;
        for (i, (pk, op_id, ts, ai_generated)) in ring.iter().enumerate().rev() {
            let r = adw::ActionRow::new();
            r.set_title(&format!("···{}", hex::encode_upper(&pk[..4])));
            let mine_marker = if pk == &me_bytes { "  (you)" } else { "" };
            let ai_marker = if *ai_generated { "  🤖 AI-assisted" } else { "" };
            r.set_subtitle(&format!(
                "edit {}  ·  {}{}{}",
                hex::encode(&op_id[..4]),
                relative_age(*ts),
                mine_marker, ai_marker,
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

/// "aries" → "Aries" — `sign_name_lower` is deliberately lowercase for
/// mid-sentence use elsewhere; UI labels here want it capitalized standalone.
/// Show/hide each `AdwActionRow` inside `group` by whether its own title
/// contains `query` (case-insensitive; empty query shows everything).
/// Recurses through descendants rather than assuming a fixed depth, since
/// `AdwPreferencesGroup`'s internal boxed-list wrapping isn't part of its
/// public contract — matching by the row's own `title()` property (already
/// set from `InterpRowInit.title` in `interp_row.rs::init_root`) avoids
/// needing to know that shape at all.
fn filter_preferences_group_rows(group: &adw::PreferencesGroup, query: &str) {
    fn walk(widget: &gtk::Widget, query: &str) {
        let mut child = widget.first_child();
        while let Some(w) = child {
            let next = w.next_sibling();
            if let Some(row) = w.downcast_ref::<adw::ActionRow>() {
                let matches = query.is_empty()
                    || row.title().to_lowercase().contains(query);
                row.set_visible(matches);
            } else {
                walk(&w, query);
            }
            child = next;
        }
    }
    walk(group.upcast_ref::<gtk::Widget>(), query);
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

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
