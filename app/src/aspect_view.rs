//! Aspect + interpretation view.
//!
//! `AspectView` is a `SimpleComponent` that renders a list of aspect /
//! placement / synastry / transit rows (via the `InterpRow` factory) and pushes
//! a detail page on row activation.  It owns its own `adw::NavigationView`.
//!
//! Construction goes through [`launch`] which spawns the component, retrieves
//! the root widget, and detaches the runtime so the caller doesn't need to
//! hold a `Controller` ref.

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
use crate::aspect_list::AspectItem;
use crate::interp_row::{InterpRow, InterpRowInit, InterpRowOut};

// ── public entry point ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum AspectViewKind {
    /// Natal chart — group title "Aspects", placements preamble.
    Natal,
    /// Sky / transits — group title "Transits", no preamble.
    Transit,
    /// Synastry between two charts — group title "Aspects", no preamble.
    Synastry,
}

pub struct AspectViewInit {
    pub kind:             AspectViewKind,
    pub items:            Vec<AspectItem>,
    /// Placement rows to render above the aspects group (Natal only).  Ignored
    /// for other kinds.
    pub placements_items: Vec<AspectItem>,
    /// No longer used after PR2 (placements come pre-built from the caller).
    /// Retained on the type for now in case a later kind needs the chart.
    #[allow(dead_code)]
    pub chart:            Option<Rc<Chart>>,
    pub store:            Rc<RefCell<ZodiaStore>>,
    pub baseline:         Rc<BaselineStore>,
    pub identity:         Rc<IdentityKeypair>,
    pub parent_sender:    AsyncComponentSender<AppModel>,
}

/// Convenience: spawn the component, return its root widget, detach the runtime
/// so the caller doesn't need to hold the `Controller`.  Idiomatic for a child
/// component embedded in a non-relm4 build function (e.g. `build_widgets`).
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
    /// Aspects factory (always present).  Owned to keep the runtime alive;
    /// drained via `send` once PR3 wires the reactive `InterpUpdated` path.
    #[allow(dead_code)]
    rows:             FactoryVecDeque<InterpRow>,
    /// Placements factory (Natal only; empty / unused otherwise).
    #[allow(dead_code)]
    placements_rows:  Option<FactoryVecDeque<InterpRow>>,
}

#[derive(Debug)]
pub enum AspectViewMsg {
    OpenDetail {
        key:             InterpKey,
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
        // ── Aspects factory ───────────────────────────────────────────────────
        let row_inits: Vec<InterpRowInit> = init.items.iter()
            .map(|it| build_row_init(it, &init.store.borrow(), &init.baseline))
            .collect();
        let items_empty = row_inits.is_empty();

        let mut rows: FactoryVecDeque<InterpRow> = FactoryVecDeque::builder()
            .launch(adw::PreferencesGroup::new())
            .forward(sender.input_sender(), |out| match out {
                InterpRowOut::Activate { key, transit_context } =>
                    AspectViewMsg::OpenDetail { key, transit_context },
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

        // ── Placements factory (Natal only) ───────────────────────────────────
        let placements_rows: Option<FactoryVecDeque<InterpRow>> =
            if matches!(init.kind, AspectViewKind::Natal) && !init.placements_items.is_empty() {
                let p_inits: Vec<InterpRowInit> = init.placements_items.iter()
                    .map(|it| build_row_init(it, &init.store.borrow(), &init.baseline))
                    .collect();
                let mut p_rows: FactoryVecDeque<InterpRow> = FactoryVecDeque::builder()
                    .launch(adw::PreferencesGroup::new())
                    .forward(sender.input_sender(), |out| match out {
                        InterpRowOut::Activate { key, transit_context } =>
                            AspectViewMsg::OpenDetail { key, transit_context },
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

        // Prepend placements group above the aspects group.
        if let Some(p_rows) = model.placements_rows.as_ref() {
            widgets.content_box.prepend(p_rows.widget());
        }

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            AspectViewMsg::OpenDetail { key, transit_context } => {
                let page = detail_page(
                    &key,
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

/// Build the interpretation detail page for `key`.
///
/// `transit_context` is an optional human-readable date-range string
/// (e.g. `"Active: 8 Apr – 16 Apr 2026"`) shown at the top of the page for
/// transit aspects.  Pass `None` for natal/synastry pages.
pub fn detail_page(
    key: &InterpKey,
    transit_context: Option<String>,
    store: Rc<RefCell<ZodiaStore>>,
    baseline: Rc<BaselineStore>,
    identity: Rc<IdentityKeypair>,
    sender: AsyncComponentSender<AppModel>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();

    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let title = gtk::Label::new(Some(&key.plain_name()));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));
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

    // ── transit timing ────────────────────────────────────────────────────────
    if let Some(ctx) = transit_context {
        let timing_group = adw::PreferencesGroup::new();
        timing_group.set_title("Timing");
        let timing_row = adw::ActionRow::new();
        timing_row.set_title(&ctx);
        let icon = gtk::Image::from_icon_name("x-office-calendar-symbolic");
        icon.add_css_class("dim-label");
        timing_row.add_prefix(&icon);
        timing_group.add(&timing_row);
        content.append(&timing_group);
    }

    // ── existing interpretations ──────────────────────────────────────────────

    let interp_group = adw::PreferencesGroup::new();
    interp_group.set_title("Interpretations");

    // Community entries from DB + in-memory baseline appended at the end.
    let mut existing = store.borrow().all_for_key(key).unwrap_or_default();
    if !existing.iter().any(|r| r.is_baseline) {
        if let Some(row) = baseline.row_for_key(key) {
            existing.push(row);
        }
    }

    if existing.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title("No interpretations yet");
        row.set_subtitle("Be the first to contribute below.");
        interp_group.add(&row);
    } else {
        for row_data in &existing {
            let interp_row = adw::ActionRow::new();
            interp_row.set_title(&row_data.body);
            interp_row.set_subtitle(&if row_data.is_baseline {
                format!("Baseline  ·  {} ♡", row_data.affirmation_count)
            } else {
                format!("{} ♡  ·  community", row_data.affirmation_count)
            });

            let affirm_btn = gtk::Button::from_icon_name("emblem-favorite-symbolic");
            affirm_btn.add_css_class("flat");
            affirm_btn.set_valign(gtk::Align::Center);

            if row_data.is_baseline {
                affirm_btn.set_sensitive(false);
                affirm_btn.set_tooltip_text(Some("Baseline — not affirmable"));
            } else {
                affirm_btn.set_tooltip_text(Some("Affirm this interpretation"));
                let store_c = Rc::clone(&store);
                let identity_c = Rc::clone(&identity);
                let log_id = row_data.log_id;
                let row_ref = interp_row.clone();
                affirm_btn.connect_clicked(move |_| {
                    let author_pk = identity_c.public_key();
                    if let Ok(true) = store_c.borrow().affirm(&log_id, &author_pk) {
                        if let Ok(n) = store_c.borrow().affirmation_count(&log_id) {
                            row_ref.set_subtitle(&format!("{n} ♡  ·  affirmed"));
                        }
                    }
                });
            }

            interp_row.add_suffix(&affirm_btn);
            interp_group.add(&interp_row);
        }
    }

    content.append(&interp_group);

    // ── contribute ────────────────────────────────────────────────────────────

    let contribute_group = adw::PreferencesGroup::new();
    contribute_group.set_title("Contribute");
    contribute_group.set_description(Some(
        "Optional — add your own reading if you have one.",
    ));

    let entry = adw::EntryRow::new();
    entry.set_title("Your interpretation…");
    contribute_group.add(&entry);
    content.append(&contribute_group);

    let submit = gtk::Button::with_label("Share");
    submit.add_css_class("flat");
    submit.set_halign(gtk::Align::End);
    submit.set_margin_top(4);

    let store_c = Rc::clone(&store);
    let identity_c = Rc::clone(&identity);
    let entry_c = entry.clone();
    let key_c = key.clone();
    let group_c = interp_group.clone();
    let sender_c = sender.clone();
    submit.connect_clicked(move |_| {
        let text = entry_c.text().to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let payload = ZodiaStore::signing_payload(&key_c, trimmed);
        let author_sig = identity_c.sign(&payload);
        let author_pk = identity_c.public_key();
        if let Ok(_log_id) = store_c.borrow().insert_signed(&key_c, trimmed, &author_pk, &author_sig) {
            let new_row = adw::ActionRow::new();
            new_row.set_title(trimmed);
            new_row.set_subtitle("0 ♡  ·  community (just added)");
            group_c.add(&new_row);
            entry_c.set_text("");
            sender_c.input(AppMsg::ShareInterp(InterpEntry {
                interp_key: key_c.to_sig(),
                body: trimmed.to_string(),
                author_pk,
                author_sig: author_sig.to_vec(),
            }));
        }
    });

    content.append(&submit);
    clamp.set_child(Some(&content));
    scroll.set_child(Some(&clamp));
    toolbar.set_content(Some(&scroll));

    adw::NavigationPage::new(&toolbar, &key.plain_name())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Best interpretation text for `key`: community DB result first, baseline fallback.
fn resolve_top_body(store: &ZodiaStore, baseline: &BaselineStore, key: &InterpKey) -> String {
    store.top_body(key).ok().flatten()
        .or_else(|| baseline.lookup(key).map(str::to_owned))
        .unwrap_or_default()
}

/// Convert an `AspectItem` into the factory's `InterpRowInit`, resolving the
/// best available body preview from the community store + baseline.
fn build_row_init(it: &AspectItem, store: &ZodiaStore, baseline: &BaselineStore) -> InterpRowInit {
    InterpRowInit {
        key:             it.key.clone(),
        title:           it.key.plain_name(),
        symbol_line:     Some(it.symbol_line.clone()),
        meta_line:       it.meta_line.clone(),
        transit_context: it.transit_context.clone(),
        body_preview:    resolve_top_body(store, baseline, &it.key),
    }
}
