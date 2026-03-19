//! Aspect list — one `adw::ActionRow` per astrological aspect, each tappable
//! to open an interpretation dialog (view, affirm, contribute).

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use zodia_core::{Aspect, HouseTransit, InterpKey, TransitAspect};
use zodia_store::ZodiaStore;

// ── row data ──────────────────────────────────────────────────────────────────

/// Data for a single aspect list row.
pub struct AspectItem {
    pub key: InterpKey,
    /// Compact glyph string, e.g. "☽△♀  orb 2.3°" or "♄  →  ⌂7"
    pub glyph_suffix: String,
}

pub fn natal_items(aspects: &[Aspect]) -> Vec<AspectItem> {
    aspects
        .iter()
        .map(|a| AspectItem {
            key: InterpKey::from_natal(a),
            glyph_suffix: format!(
                "{}{}{}  orb {:.1}°",
                a.body_a.symbol(), a.kind.symbol(), a.body_b.symbol(), a.orb
            ),
        })
        .collect()
}

pub fn transit_items(
    transits: &[TransitAspect],
    house_transits: &[HouseTransit],
) -> Vec<AspectItem> {
    let mut items: Vec<AspectItem> = transits
        .iter()
        .map(|ta| AspectItem {
            key: ta.interp_key(),
            glyph_suffix: format!(
                "{}{}natal {}  orb {:.1}°",
                ta.transiting.symbol(), ta.kind.symbol(), ta.natal_body.symbol(), ta.orb
            ),
        })
        .collect();

    for ht in house_transits {
        items.push(AspectItem {
            key: ht.interp_key(),
            glyph_suffix: format!("{}  →  ⌂{}", ht.transiting.symbol(), ht.house),
        });
    }
    items
}

// ── list builder ──────────────────────────────────────────────────────────────

/// Build a `gtk::ListBox` (boxed-list style) from a slice of aspect items.
///
/// Each row opens an interpretation dialog on activation.  The `parent`
/// window is used as the dialog's `transient-for` target.
pub fn build_list(
    items: Vec<AspectItem>,
    store: Rc<RefCell<ZodiaStore>>,
    author_pk: [u8; 32],
    parent: &adw::ApplicationWindow,
) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    if items.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title("No aspects within default orbs");
        list.append(&row);
        return list;
    }

    for item in items {
        let top_body = store
            .borrow()
            .top_body(&item.key)
            .ok()
            .flatten()
            .unwrap_or_default();

        let row = adw::ActionRow::new();
        row.set_title(&item.key.plain_name());

        // Subtitle: interpretation excerpt, or a dimmed placeholder
        if top_body.is_empty() {
            row.set_subtitle("No interpretation yet — tap to contribute");
        } else {
            row.set_subtitle(&truncate(&top_body, 90));
        }

        // Right-hand glyph label
        let glyph_lbl = gtk::Label::new(Some(&item.glyph_suffix));
        glyph_lbl.add_css_class("dim-label");
        glyph_lbl.add_css_class("caption");
        glyph_lbl.add_css_class("aspect-list");
        row.add_suffix(&glyph_lbl);

        // Chevron affordance
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.set_activatable(true);

        // Tap → interpretation dialog
        let store_c = Rc::clone(&store);
        let key = item.key.clone();
        let parent_c = parent.clone();
        row.connect_activated(move |_| {
            show_interp_dialog(&parent_c, key.clone(), Rc::clone(&store_c), author_pk);
        });

        list.append(&row);
    }

    list
}

// ── interpretation dialog ─────────────────────────────────────────────────────

fn show_interp_dialog(
    parent: &adw::ApplicationWindow,
    key: InterpKey,
    store: Rc<RefCell<ZodiaStore>>,
    author_pk: [u8; 32],
) {
    let dialog = adw::Window::builder()
        .modal(true)
        .transient_for(parent)
        .default_width(480)
        .default_height(560)
        .title(&key.plain_name())
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let title_lbl = gtk::Label::new(Some(&key.plain_name()));
    title_lbl.add_css_class("title");
    header.set_title_widget(Some(&title_lbl));
    toolbar.add_top_bar(&header);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_vexpand(true);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(440);
    clamp.set_margin_top(16);
    clamp.set_margin_bottom(24);
    clamp.set_margin_start(16);
    clamp.set_margin_end(16);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);

    // ── existing interpretations ──────────────────────────────────────────────

    let interp_group = adw::PreferencesGroup::new();
    interp_group.set_title("Interpretations");

    let existing = store.borrow().all_for_key(&key).unwrap_or_default();

    if existing.is_empty() {
        let placeholder = adw::ActionRow::new();
        placeholder.set_title("No interpretations yet");
        placeholder.set_subtitle("Be the first to contribute below.");
        interp_group.add(&placeholder);
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
            let log_id = row_data.log_id;
            let row_ref = interp_row.clone();
            affirm_btn.connect_clicked(move |_| {
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
    let entry_c = entry.clone();
    let key_c = key.clone();
    let group_c = interp_group.clone();
    submit.connect_clicked(move |_| {
        let text = entry_c.text().to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(_log_id) = store_c.borrow().insert_interpretation(
            &key_c,
            trimmed,
            Some(&author_pk),
            false,
        ) {
            // Append the new row immediately so the user sees it.
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
    dialog.set_content(Some(&toolbar));
    dialog.present();
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}
