//! Connected-peer navigation page.
//!
//! Added as a named child of the main `gtk::Stack` when a Tier-1 exchange
//! completes.  Shows three tabs:
//!   - **Their Chart** — peer's planet placements + their natal aspects.
//!   - **Synastry**    — cross-chart aspects between the two of you.
//!   - **Messages**    — live text chat over the Tier-1 QUIC channel.
//!
//! Returns `(ToolbarView, gtk::ListBox, call_btn, send_btn)` — the caller
//! appends chat rows to the `ListBox` whenever new messages arrive.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::glib;
use libadwaita::prelude::*;
use relm4::AsyncComponentSender;
use zodia_core::{Chart, compute_positions, compute_synastry};
use zodia_crypto::IdentityKeypair;
use zodia_net::{PeerId, Tier1Blob};
use zodia_store::ZodiaStore;

use crate::app::{AppModel, AppMsg};
use crate::aspect_list::{natal_items, synastry_items};
use crate::aspect_view::AspectView;
use crate::util::sign_glyph;

/// Build the `adw::ToolbarView` for a connected peer.
///
/// Returns `(toolbar_view, msg_list, call_btn, send_btn)`.
/// `call_btn` and `send_btn` should be set insensitive when the peer is offline.
pub fn build_peer_page(
    peer_id: &PeerId,
    their_blob: &Tier1Blob,
    our_chart: &Chart,
    store: Rc<RefCell<ZodiaStore>>,
    identity: Rc<IdentityKeypair>,
    sender: &AsyncComponentSender<AppModel>,
    nickname: Option<&str>,
) -> (adw::ToolbarView, gtk::ListBox, gtk::Button, gtk::Button) {
    let peer_hex = hex::encode_upper(&peer_id.0[..4]);

    // ── compute their chart + synastry ────────────────────────────────────────

    let their_chart = Chart::compute(their_blob.birth.clone()).ok();

    let synastry = match compute_positions(their_blob.birth.jdn) {
        Ok(their_pos) => compute_synastry(&our_chart.positions, &their_pos),
        Err(e) => {
            tracing::warn!(peer = %peer_hex, "synastry computation failed: {e}");
            vec![]
        }
    };

    // ── tab view ──────────────────────────────────────────────────────────────

    let tab_view = adw::TabView::new();

    // Prevent all tabs from being closed — these three are permanent.
    tab_view.connect_close_page(|view, page| {
        view.close_page_finish(page, false);
        glib::Propagation::Stop
    });

    // ── nickname entry (only on "Their Chart" tab) ────────────────────────────
    let nick_entry = adw::EntryRow::new();
    nick_entry.set_title("Nickname");
    if let Some(n) = nickname.filter(|n| !n.is_empty()) {
        nick_entry.set_text(n);
    }
    nick_entry.set_show_apply_button(true);

    let nick_group = adw::PreferencesGroup::new();
    nick_group.add(&nick_entry);

    let nick_clamp = adw::Clamp::new();
    nick_clamp.set_maximum_size(720);
    nick_clamp.set_margin_top(8);
    nick_clamp.set_margin_start(12);
    nick_clamp.set_margin_end(12);
    nick_clamp.set_child(Some(&nick_group));

    let pid_n = peer_id.clone();
    let s_n = sender.clone();
    let ne_c = nick_entry.clone();
    nick_entry.connect_apply(move |_| {
        s_n.input(AppMsg::SetNickname {
            peer_id: pid_n.clone(),
            name: ne_c.text().to_string(),
        });
    });

    // Their Chart tab — nick entry + aspect view stacked
    let their_av = match &their_chart {
        Some(chart) => AspectView::natal(
            natal_items(&chart.natal_aspects()),
            chart,
            Rc::clone(&store),
            Rc::clone(&identity),
        ),
        None => AspectView::new(vec![], Rc::clone(&store), Rc::clone(&identity)),
    };
    their_av.widget().set_vexpand(true);

    let their_tab = gtk::Box::new(gtk::Orientation::Vertical, 0);
    their_tab.append(&nick_clamp);
    their_tab.append(their_av.widget());
    their_tab.set_vexpand(true);

    let their_page = tab_view.append(&their_tab);
    their_page.set_title("Their Chart");

    // Synastry tab
    let syn_av = AspectView::new(synastry_items(&synastry), Rc::clone(&store), Rc::clone(&identity));
    syn_av.widget().set_vexpand(true);
    let syn_page = tab_view.append(syn_av.widget());
    syn_page.set_title("Synastry");

    // Messages tab — call and send buttons both live in the input row
    let (messages_widget, msg_list, call_btn, send_btn) = build_messages_tab(peer_id, sender);
    messages_widget.set_vexpand(true);
    let msg_page = tab_view.append(&messages_widget);
    msg_page.set_title("Messages");

    // Scroll to bottom whenever the Messages tab is selected — handles both
    // the initial backfill (rows added before layout) and returning to the tab.
    {
        let msg_page_ref = msg_page.clone();
        let list = msg_list.clone();
        tab_view.connect_notify_local(Some("selected-page"), move |tv, _| {
            if let Some(selected) = tv.selected_page() {
                if selected == msg_page_ref {
                    if let Some(sw) = list.parent()
                        .and_then(|p| p.downcast::<gtk::ScrolledWindow>().ok())
                    {
                        let adj = sw.vadjustment();
                        glib::idle_add_local_once(move || {
                            adj.set_value(adj.upper() - adj.page_size());
                        });
                    }
                }
            }
        });
    }

    let _ = (their_page, syn_page, msg_page);

    // ── toolbar view ──────────────────────────────────────────────────────────

    let toolbar_view = adw::ToolbarView::new();

    // Header bar — nickname (or peer hex) as the title
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let their_solar_month = zodia_core::solar_month(their_blob.birth.jdn);
    let glyph = sign_glyph(their_solar_month);
    let title_text = nickname
        .filter(|n| !n.is_empty())
        .map(|n| format!("{glyph}  {n}"))
        .unwrap_or_else(|| format!("{glyph}  ···{peer_hex}"));
    let win_title = adw::WindowTitle::new(&title_text, "");
    header.set_title_widget(Some(&win_title));

    // Tab bar sits directly below the header
    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&tab_view));
    tab_bar.set_autohide(false);

    toolbar_view.add_top_bar(&header);
    toolbar_view.add_top_bar(&tab_bar);
    toolbar_view.set_content(Some(&tab_view));

    (toolbar_view, msg_list, call_btn, send_btn)
}

// ── messages tab ──────────────────────────────────────────────────────────────

/// Build the Messages tab content.
///
/// Returns `(container_widget, msg_list, call_btn, send_btn)`.
/// Both action buttons live in the input row so they are always accessible.
fn build_messages_tab(
    peer_id: &PeerId,
    sender: &AsyncComponentSender<AppModel>,
) -> (gtk::Box, gtk::ListBox, gtk::Button, gtk::Button) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_vexpand(true);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(720);
    clamp.set_vexpand(true);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.set_vexpand(true);

    // Scrollable message list
    let msg_list = gtk::ListBox::new();
    msg_list.set_selection_mode(gtk::SelectionMode::None);
    msg_list.add_css_class("boxed-list");
    msg_list.set_vexpand(true);

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scrolled.set_child(Some(&msg_list));
    vbox.append(&scrolled);

    // Input row — call button | text entry | send button
    let input_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    input_row.set_margin_start(12);
    input_row.set_margin_end(12);
    input_row.set_margin_top(8);
    input_row.set_margin_bottom(12);

    let call_btn = gtk::Button::from_icon_name("call-start-symbolic");
    call_btn.add_css_class("circular");
    call_btn.set_tooltip_text(Some("Start voice call"));
    let pid_c = peer_id.clone();
    let sc = sender.clone();
    call_btn.connect_clicked(move |_| sc.input(AppMsg::CallPeer(pid_c.clone())));
    input_row.append(&call_btn);

    let entry = gtk::Entry::new();
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some("Message…"));
    input_row.append(&entry);

    let send_btn = gtk::Button::from_icon_name("mail-send-symbolic");
    send_btn.add_css_class("suggested-action");
    send_btn.add_css_class("circular");
    send_btn.set_tooltip_text(Some("Send"));
    input_row.append(&send_btn);

    vbox.append(&input_row);

    clamp.set_child(Some(&vbox));
    outer.append(&clamp);

    // Wire send button + Enter key
    let pid = peer_id.clone();
    let s = sender.clone();
    let entry_c = entry.clone();
    let send = move || {
        let text = entry_c.text().trim().to_string();
        if !text.is_empty() {
            s.input(AppMsg::SendChat { peer_id: pid.clone(), text });
            entry_c.set_text("");
        }
    };

    let send_c = send.clone();
    send_btn.connect_clicked(move |_| send_c());
    entry.connect_activate(move |_| send());

    (outer, msg_list, call_btn, send_btn)
}

/// Append a single chat row to a message list.
///
/// `from_us = true`  → right-aligned "you" bubble style.
/// `from_us = false` → left-aligned "them" bubble style.
pub fn append_chat_row(list: &gtk::ListBox, text: &str, from_us: bool) {
    let row = gtk::ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);

    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_xalign(if from_us { 1.0 } else { 0.0 });
    label.set_margin_start(12);
    label.set_margin_end(12);
    label.set_margin_top(6);
    label.set_margin_bottom(6);


    row.set_child(Some(&label));
    list.append(&row);

    // Defer scroll to bottom until GTK has computed layout (adj.upper may be
    // zero if the widget is not yet realized or is currently hidden).
    if let Some(sw) = list.parent()
        .and_then(|p| p.downcast::<gtk::ScrolledWindow>().ok())
    {
        let adj = sw.vadjustment();
        glib::idle_add_local_once(move || {
            adj.set_value(adj.upper() - adj.page_size());
        });
    }
}
