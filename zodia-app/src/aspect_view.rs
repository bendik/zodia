//! Aspect + interpretation view.
//!
//! An `adw::NavigationView` with two pages:
//!   1. **List page** — full-width grouped list of aspect rows.  One column of
//!      text at any window width.
//!   2. **Detail page** — pushed on row tap; shows all interpretations with
//!      affirm buttons and a contribute form.  Has its own HeaderBar so the
//!      back button is always present.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use zodia_core::InterpKey;
use zodia_crypto::IdentityKeypair;
use zodia_store::ZodiaStore;

use crate::aspect_list::AspectItem;

// ── public entry point ────────────────────────────────────────────────────────

pub struct AspectView {
    nav: adw::NavigationView,
}

impl AspectView {
    /// Standard aspect list with group title "Aspects".  Used for synastry.
    pub fn new(
        items: Vec<AspectItem>,
        store: Rc<RefCell<ZodiaStore>>,
        identity: Rc<IdentityKeypair>,
    ) -> Self {
        Self::build(items, store, identity, None, "Aspects")
    }

    /// Natal chart view — prepends a placements section above the aspect list.
    pub fn natal(
        items: Vec<AspectItem>,
        chart: &zodia_core::Chart,
        store: Rc<RefCell<ZodiaStore>>,
        identity: Rc<IdentityKeypair>,
    ) -> Self {
        let preamble = crate::placements::build_placements_group(chart);
        Self::build(items, store, identity, Some(preamble.upcast::<gtk::Widget>()), "Aspects")
    }

    /// Transit view — group title is "Transits".
    pub fn transits(
        items: Vec<AspectItem>,
        store: Rc<RefCell<ZodiaStore>>,
        identity: Rc<IdentityKeypair>,
    ) -> Self {
        Self::build(items, store, identity, None, "Transits")
    }

    fn build(
        items: Vec<AspectItem>,
        store: Rc<RefCell<ZodiaStore>>,
        identity: Rc<IdentityKeypair>,
        preamble: Option<gtk::Widget>,
        group_title: &'static str,
    ) -> Self {
        let nav = adw::NavigationView::new();
        nav.set_vexpand(true);
        nav.push(&list_page(&items, &nav, Rc::clone(&store), Rc::clone(&identity), preamble, group_title));
        Self { nav }
    }

    pub fn widget(&self) -> &adw::NavigationView {
        &self.nav
    }
}

// ── list page ─────────────────────────────────────────────────────────────────

fn list_page(
    items: &[AspectItem],
    nav: &adw::NavigationView,
    store: Rc<RefCell<ZodiaStore>>,
    identity: Rc<IdentityKeypair>,
    preamble: Option<gtk::Widget>,
    group_title: &'static str,
) -> adw::NavigationPage {
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_vexpand(true);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(720);
    clamp.set_margin_top(8);
    clamp.set_margin_bottom(8);
    clamp.set_margin_start(12);
    clamp.set_margin_end(12);

    let aspect_group = adw::PreferencesGroup::new();
    aspect_group.set_title(group_title);

    if items.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title("No aspects within default orbs");
        aspect_group.add(&row);
    } else {
        for item in items {
            let top_body = store
                .borrow()
                .top_body(&item.key)
                .ok()
                .flatten()
                .unwrap_or_default();

            let row = adw::ActionRow::new();
            row.set_title(&item.key.plain_name());
            if top_body.is_empty() {
                row.set_subtitle("No interpretation yet — tap to contribute");
            } else {
                row.set_subtitle(&truncate(&top_body, 120));
            }

            let glyph_lbl = gtk::Label::new(Some(&item.glyph_suffix));
            glyph_lbl.add_css_class("dim-label");
            glyph_lbl.add_css_class("caption");
            glyph_lbl.add_css_class("aspect-list");
            row.add_suffix(&glyph_lbl);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.set_activatable(true);

            let nav_c = nav.clone();
            let store_c = Rc::clone(&store);
            let identity_c = Rc::clone(&identity);
            let key = item.key.clone();
            row.connect_activated(move |_| {
                nav_c.push(&detail_page(&key, Rc::clone(&store_c), Rc::clone(&identity_c)));
            });

            aspect_group.add(&row);
        }
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    if let Some(w) = preamble {
        content.append(&w);
    }
    content.append(&aspect_group);
    clamp.set_child(Some(&content));
    scroll.set_child(Some(&clamp));
    adw::NavigationPage::new(&scroll, "Aspects")
}

// ── detail page ───────────────────────────────────────────────────────────────

/// Build the interpretation detail page for `key`.
pub fn detail_page(
    key: &InterpKey,
    store: Rc<RefCell<ZodiaStore>>,
    identity: Rc<IdentityKeypair>,
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

    // ── existing interpretations ──────────────────────────────────────────────

    let interp_group = adw::PreferencesGroup::new();
    interp_group.set_title("Interpretations");

    let existing = store.borrow().all_for_key(key).unwrap_or_default();

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
            affirm_btn.set_tooltip_text(Some("Affirm this interpretation"));
            affirm_btn.set_valign(gtk::Align::Center);

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

            interp_row.add_suffix(&affirm_btn);
            interp_group.add(&interp_row);
        }
    }

    content.append(&interp_group);

    // ── contribute ────────────────────────────────────────────────────────────

    let contribute_group = adw::PreferencesGroup::new();
    contribute_group.set_title("Contribute");
    contribute_group.set_description(Some(
        "Share your lived understanding of this placement.",
    ));

    let entry = adw::EntryRow::new();
    entry.set_title("Your interpretation…");
    contribute_group.add(&entry);
    content.append(&contribute_group);

    let submit = gtk::Button::with_label("Submit  ✓");
    submit.add_css_class("suggested-action");
    submit.add_css_class("pill");
    submit.set_halign(gtk::Align::End);
    submit.set_margin_top(4);

    let store_c = Rc::clone(&store);
    let identity_c = Rc::clone(&identity);
    let entry_c = entry.clone();
    let key_c = key.clone();
    let group_c = interp_group.clone();
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
        }
    });

    content.append(&submit);
    clamp.set_child(Some(&content));
    scroll.set_child(Some(&clamp));
    toolbar.set_content(Some(&scroll));

    adw::NavigationPage::new(&toolbar, &key.plain_name())
}

// ── helpers ───────────────────────────────────────────────────────────────────

pub(crate) fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}
