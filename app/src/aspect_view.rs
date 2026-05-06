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
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use relm4::{AsyncComponentSender, Component};
use zodia_core::{Chart, InterpKey};
use zodia_crypto::IdentityKeypair;
use zodia_net::InterpEntry;
use zodia_store::{ZodiaStore, BaselineStore};

use crate::app::{AppModel, AppMsg};
use crate::aspect_list::{AspectItem, KeyEntry};
use crate::interp_row::{InterpRow, InterpRowInit, InterpRowOut};

// ── public entry point ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum AspectViewKind {
    Natal,
    Transit,
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
    pub store:            Rc<RefCell<ZodiaStore>>,
    pub baseline:         Rc<BaselineStore>,
    pub identity:         Rc<IdentityKeypair>,
    pub parent_sender:    AsyncComponentSender<AppModel>,
}

/// Spawn the component and return its root widget; runtime detached so the
/// caller doesn't have to hold a `Controller`.
pub fn launch(init: AspectViewInit) -> adw::NavigationView {
    let mut ctl = <AspectView as Component>::builder().launch(init).detach();
    let widget = ctl.widget().clone();
    ctl.detach_runtime();
    widget
}

// ── component ─────────────────────────────────────────────────────────────────

pub struct AspectView {
    nav:              adw::NavigationView,
    store:            Rc<RefCell<ZodiaStore>>,
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

#[relm4::component(pub)]
impl SimpleComponent for AspectView {
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

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Aspects factory.
        let row_inits: Vec<InterpRowInit> = init.items.iter()
            .map(|it| build_row_init(it, &init.store.borrow(), &init.baseline))
            .collect();
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

        let group_title = match init.kind {
            AspectViewKind::Natal | AspectViewKind::Synastry => "Aspects",
            AspectViewKind::Transit                          => "Transits",
        };
        let interp_group = rows.widget().clone();
        interp_group.set_title(group_title);

        // Placements factory (Natal only).
        let placements_rows: Option<FactoryVecDeque<InterpRow>> =
            if matches!(init.kind, AspectViewKind::Natal) && !init.placements_items.is_empty() {
                let p_inits: Vec<InterpRowInit> = init.placements_items.iter()
                    .map(|it| build_row_init(it, &init.store.borrow(), &init.baseline))
                    .collect();
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

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            AspectViewMsg::OpenDetail { keys, transit_context } => {
                let page = detail_page(
                    &keys,
                    transit_context,
                    Rc::clone(&self.store),
                    Rc::clone(&self.baseline),
                    Rc::clone(&self.identity),
                    self.parent_sender.clone(),
                );
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
pub fn detail_page(
    keys: &[KeyEntry],
    transit_context: Option<String>,
    store: Rc<RefCell<ZodiaStore>>,
    baseline: Rc<BaselineStore>,
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

    // ── interpretations: one labelled group per key, all visible ──────────────
    // We need to track each group by key sig so the contribute submit can
    // append to the right one without rebuilding the page.
    let mut groups_by_sig: std::collections::HashMap<String, adw::PreferencesGroup> =
        std::collections::HashMap::new();
    for entry in keys {
        let group = build_interpretations_group(
            &entry.key,
            &store.borrow(),
            &baseline,
            Some(Rc::clone(&identity)),
            Some(Rc::clone(&store)),
        );
        // For 1 key, "Interpretations" is the right title.  For 2+, label per key.
        if keys.len() > 1 {
            group.set_title(&format!("{} — Interpretations", entry.label));
        }
        groups_by_sig.insert(entry.key.to_sig(), group.clone());
        content.append(&group);
    }

    // ── contribute (single, with target-key radio when multi-key) ─────────────
    let contribute = build_contribute_group(
        keys,
        Rc::clone(&store),
        Rc::clone(&identity),
        sender,
        groups_by_sig,
    );
    content.append(&contribute);

    clamp.set_child(Some(&content));
    scroll.set_child(Some(&clamp));
    toolbar.set_content(Some(&scroll));

    adw::NavigationPage::new(&toolbar, &page_title)
}

/// Build the Interpretations group for `key`.  When `identity` + `store_rc`
/// are provided, community rows show an active affirm button.  When `None`
/// (Combined page), affirm is omitted.
fn build_interpretations_group(
    key: &InterpKey,
    store: &ZodiaStore,
    baseline: &BaselineStore,
    identity: Option<Rc<IdentityKeypair>>,
    store_rc: Option<Rc<RefCell<ZodiaStore>>>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("Interpretations");

    let mut existing = store.all_for_key(key).unwrap_or_default();
    if !existing.iter().any(|r| r.is_baseline) {
        if let Some(row) = baseline.row_for_key(key) {
            existing.push(row);
        }
    }

    if existing.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title("No interpretations yet");
        row.set_subtitle("Be the first to contribute below.");
        group.add(&row);
    } else {
        for row_data in &existing {
            let r = adw::ActionRow::new();
            r.set_title(&row_data.body);
            r.set_subtitle(&if row_data.is_baseline {
                format!("Baseline  ·  {} ♡", row_data.affirmation_count)
            } else {
                format!("{} ♡  ·  community", row_data.affirmation_count)
            });

            if let (Some(ident), Some(st)) = (identity.as_ref(), store_rc.as_ref()) {
                let affirm_btn = gtk::Button::from_icon_name("emblem-favorite-symbolic");
                affirm_btn.add_css_class("flat");
                affirm_btn.set_valign(gtk::Align::Center);
                if row_data.is_baseline {
                    affirm_btn.set_sensitive(false);
                    affirm_btn.set_tooltip_text(Some("Baseline — not affirmable"));
                } else {
                    affirm_btn.set_tooltip_text(Some("Affirm this interpretation"));
                    let store_c    = Rc::clone(st);
                    let identity_c = Rc::clone(ident);
                    let log_id     = row_data.log_id;
                    let row_ref    = r.clone();
                    affirm_btn.connect_clicked(move |_| {
                        let author_pk = identity_c.public_key();
                        if let Ok(true) = store_c.borrow().affirm(&log_id, &author_pk) {
                            if let Ok(n) = store_c.borrow().affirmation_count(&log_id) {
                                row_ref.set_subtitle(&format!("{n} ♡  ·  affirmed"));
                            }
                        }
                    });
                }
                r.add_suffix(&affirm_btn);
            }

            group.add(&r);
        }
    }

    group
}

/// Build the Contribute group.  Single text entry + Share button.  When
/// `keys.len() > 1`, a radio row picks which key the contribution targets;
/// otherwise the lone key is implicit.  Submitting appends a new row to the
/// matching Interpretations group so the user sees their addition immediately.
fn build_contribute_group(
    keys: &[KeyEntry],
    store: Rc<RefCell<ZodiaStore>>,
    identity: Rc<IdentityKeypair>,
    sender: AsyncComponentSender<AppModel>,
    groups_by_sig: std::collections::HashMap<String, adw::PreferencesGroup>,
) -> gtk::Box {
    use std::cell::Cell;

    let wrapper = gtk::Box::new(gtk::Orientation::Vertical, 4);

    let group = adw::PreferencesGroup::new();
    group.set_title("Contribute");
    group.set_description(Some("Optional — add your own reading if you have one."));

    // Tracks which key index in `keys` is currently selected.
    let selected: Rc<Cell<usize>> = Rc::new(Cell::new(0));

    // Target-key radio row (only visible for multi-key rows).
    if keys.len() > 1 {
        let target_row = adw::ActionRow::new();
        target_row.set_title("Reading for");
        target_row.set_subtitle("Which placement does this contribution describe?");

        let radio_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        radio_box.set_valign(gtk::Align::Center);

        let mut first: Option<gtk::CheckButton> = None;
        for (i, entry) in keys.iter().enumerate() {
            let cb = gtk::CheckButton::with_label(&entry.label);
            if let Some(ref f) = first {
                cb.set_group(Some(f));
            } else {
                first = Some(cb.clone());
            }
            if i == 0 { cb.set_active(true); }
            let sel = Rc::clone(&selected);
            cb.connect_toggled(move |btn| {
                if btn.is_active() { sel.set(i); }
            });
            radio_box.append(&cb);
        }

        target_row.add_suffix(&radio_box);
        group.add(&target_row);
    }

    let entry = adw::EntryRow::new();
    entry.set_title("Your interpretation…");
    group.add(&entry);
    wrapper.append(&group);

    let submit = gtk::Button::with_label("Share");
    submit.add_css_class("flat");
    submit.set_halign(gtk::Align::End);
    submit.set_margin_top(4);

    let keys_owned: Vec<KeyEntry> = keys.to_vec();
    let groups     = groups_by_sig;
    let store_c    = Rc::clone(&store);
    let identity_c = Rc::clone(&identity);
    let entry_c    = entry.clone();
    let sender_c   = sender.clone();
    let sel        = Rc::clone(&selected);
    submit.connect_clicked(move |_| {
        let text    = entry_c.text().to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() { return; }

        let idx = sel.get().min(keys_owned.len().saturating_sub(1));
        let key = match keys_owned.get(idx) { Some(k) => &k.key, None => return };

        let payload    = ZodiaStore::signing_payload(key, trimmed);
        let author_sig = identity_c.sign(&payload);
        let author_pk  = identity_c.public_key();
        if let Ok(_log_id) = store_c.borrow()
            .insert_signed(key, trimmed, &author_pk, &author_sig)
        {
            if let Some(group) = groups.get(&key.to_sig()) {
                let new_row = adw::ActionRow::new();
                new_row.set_title(trimmed);
                new_row.set_subtitle("0 ♡  ·  community (just added)");
                group.add(&new_row);
            }
            entry_c.set_text("");
            sender_c.input(AppMsg::ShareInterp(InterpEntry {
                interp_key: key.to_sig(),
                body: trimmed.to_string(),
                author_pk,
                author_sig: author_sig.to_vec(),
            }));
        }
    });

    wrapper.append(&submit);
    wrapper
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Best interpretation text for `key`: community DB result first, baseline fallback.
fn resolve_top_body(store: &ZodiaStore, baseline: &BaselineStore, key: &InterpKey) -> String {
    store.top_body(key).ok().flatten()
        .or_else(|| baseline.lookup(key).map(str::to_owned))
        .unwrap_or_default()
}

/// Convert an `AspectItem` into the factory's `InterpRowInit`.  Body preview is
/// the first non-empty body across the row's keys (so a placement row shows
/// whichever of sign/house has content).
fn build_row_init(it: &AspectItem, store: &ZodiaStore, baseline: &BaselineStore) -> InterpRowInit {
    let body_preview = it.keys.iter()
        .map(|e| resolve_top_body(store, baseline, &e.key))
        .find(|b| !b.is_empty())
        .unwrap_or_default();
    InterpRowInit {
        keys:            it.keys.clone(),
        title:           it.title.clone(),
        symbol_line:     Some(it.symbol_line.clone()),
        meta_line:       it.meta_line.clone(),
        transit_context: it.transit_context.clone(),
        body_preview,
    }
}
