//! Connected-stargazer navigation page.
//!
//! Added as a named child of the main `gtk::Stack` when a Tier-1 exchange
//! completes.  Shows three tabs:
//!   - **Their Chart** — stargazer's planet placements + their natal aspects.
//!   - **Synastry**    — cross-chart aspects between the two of you.
//!   - **Messages**    — live text chat over the Tier-1 QUIC channel.
//!
//! Returns `(ToolbarView, gtk::ListBox, call_btn, send_btn)` — the caller
//! appends chat rows to the `ListBox` whenever new messages arrive.
//!
//! `ViewSwitcherTitle` is deprecated in ADW 1.4 but the TabBar alternative
//! exposes close buttons that cannot be hidden without fragile CSS hacks.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::glib;
use libadwaita::prelude::*;
use relm4::AsyncComponentSender;
use zodia_core::{Chart, compute_positions, compute_synastry};
use zodia_crypto::IdentityKeypair;
use zodia_net::{ConsentBlob, PeerId};
use zodia_store::{ZodiaStore, BaselineStore};

use crate::app::{AppModel, AppMsg};
use crate::aspect_list::{natal_items, synastry_items};
use crate::aspect_view::AspectView;
use crate::util::sign_glyph;

/// Build the `adw::ToolbarView` for a connected stargazer.
///
/// Returns `(toolbar_view, msg_list, call_btn, send_btn, entry, switcher_title)`.
/// `call_btn` and `send_btn` should be set insensitive when the stargazer is offline.
/// `switcher_title` is retained by the caller so the title can be updated when
/// the nickname changes.
#[allow(deprecated)] // ViewSwitcherTitle deprecated in ADW 1.4
pub fn build_stargazer_page(
    peer_id: &PeerId,
    their_blob: &ConsentBlob,
    our_chart: &Chart,
    store: Rc<RefCell<ZodiaStore>>,
    baseline: Rc<BaselineStore>,
    identity: Rc<IdentityKeypair>,
    sender: &AsyncComponentSender<AppModel>,
    nickname: Option<&str>,
    split_view: &adw::OverlaySplitView,
) -> (adw::ToolbarView, gtk::ListBox, gtk::Button, gtk::Button, gtk::Entry, adw::ViewSwitcherTitle) {
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

    // ── view stack ────────────────────────────────────────────────────────────

    let view_stack = adw::ViewStack::new();
    view_stack.set_vexpand(true);

    // Their Chart tab
    let their_av = match &their_chart {
        Some(chart) => AspectView::natal(
            natal_items(&chart.natal_aspects()),
            chart,
            Rc::clone(&store),
            Rc::clone(&baseline),
            Rc::clone(&identity),
            sender.clone(),
        ),
        None => AspectView::new(vec![], Rc::clone(&store), Rc::clone(&baseline), Rc::clone(&identity), sender.clone()),
    };
    their_av.widget().set_vexpand(true);

    let their_tab = gtk::Box::new(gtk::Orientation::Vertical, 0);
    their_tab.append(their_av.widget());
    their_tab.set_vexpand(true);

    let their_page = view_stack.add_titled(&their_tab, Some("their"), "Their Chart");
    their_page.set_icon_name(Some("weather-clear-symbolic"));

    // Synastry tab
    let syn_av = AspectView::new(synastry_items(&synastry), Rc::clone(&store), Rc::clone(&baseline), Rc::clone(&identity), sender.clone());
    syn_av.widget().set_vexpand(true);
    let syn_page = view_stack.add_titled(syn_av.widget(), Some("synastry"), "Synastry");
    syn_page.set_icon_name(Some("synastry-symbolic"));

    // Messages tab
    let (messages_widget, msg_list, call_btn, send_btn, entry) = build_messages_tab(peer_id, sender);
    messages_widget.set_vexpand(true);
    let msg_page = view_stack.add_titled(&messages_widget, Some("messages"), "Messages");
    msg_page.set_icon_name(Some("chat-message-new-symbolic"));

    // Scroll to bottom whenever the Messages tab becomes visible.
    {
        let list = msg_list.clone();
        view_stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
            if stack.visible_child_name().as_deref() == Some("messages") {
                if let Some(sw) = list.parent()
                    .and_then(|p| p.downcast::<gtk::ScrolledWindow>().ok())
                {
                    let adj = sw.vadjustment();
                    glib::idle_add_local_once(move || {
                        adj.set_value(adj.upper() - adj.page_size());
                    });
                }
            }
        });
    }

    let _ = (their_page, syn_page, msg_page);

    // ── toolbar view ──────────────────────────────────────────────────────────

    let toolbar_view = adw::ToolbarView::new();

    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    // Sidebar toggle — visible only when the split view is collapsed.
    // On non-macOS: placed on the left (start).
    // On macOS: placed on the right (end) to avoid the traffic-light buttons.
    let sidebar_btn = gtk::Button::from_icon_name("open-menu-symbolic");
    sidebar_btn.set_tooltip_text(Some("Show sidebar"));
    sidebar_btn.set_visible(split_view.is_collapsed());
    {
        let sv = split_view.clone();
        let btn = sidebar_btn.clone();
        split_view.connect_notify_local(Some("collapsed"), move |sv2, _| {
            btn.set_visible(sv2.is_collapsed());
        });
        sidebar_btn.connect_clicked(move |_| sv.set_show_sidebar(true));
    }
    #[cfg(not(target_os = "macos"))]
    header.pack_start(&sidebar_btn);
    #[cfg(target_os = "macos")]
    header.pack_end(&sidebar_btn);

    let their_solar_month = zodia_core::solar_month(their_blob.birth.jdn);
    let glyph = sign_glyph(their_solar_month);

    let switcher_title = adw::ViewSwitcherTitle::new();
    switcher_title.set_stack(Some(&view_stack));
    let title_text = nickname
        .filter(|n| !n.is_empty())
        .map(|n| format!("{glyph}  {n}"))
        .unwrap_or_else(|| format!("{glyph}  ···{peer_hex}"));
    switcher_title.set_title(&title_text);
    header.set_title_widget(Some(&switcher_title));

    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&view_stack));

    // Bottom switcher bar (appears when window is too narrow for header tabs)
    let switcher_bar = adw::ViewSwitcherBar::new();
    switcher_bar.set_stack(Some(&view_stack));
    switcher_title
        .bind_property("title-visible", &switcher_bar, "reveal")
        .sync_create()
        .build();
    toolbar_view.add_bottom_bar(&switcher_bar);

    (toolbar_view, msg_list, call_btn, send_btn, entry, switcher_title)
}

// ── messages tab ──────────────────────────────────────────────────────────────

/// Build the Messages tab content.
///
/// Returns `(container_widget, msg_list, call_btn, send_btn, entry)`.
/// Both action buttons live in the input row so they are always accessible.
fn build_messages_tab(
    peer_id: &PeerId,
    sender: &AsyncComponentSender<AppModel>,
) -> (gtk::Box, gtk::ListBox, gtk::Button, gtk::Button, gtk::Entry) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_vexpand(true);

    // Scrolled window is full-width so the scrollbar sits at the screen edge.
    // The list content is clamped to 720 px to match the aspect list width.
    let msg_list = gtk::ListBox::new();
    msg_list.set_selection_mode(gtk::SelectionMode::None);
    msg_list.add_css_class("boxed-list");
    msg_list.set_vexpand(true);

    let msg_clamp = adw::Clamp::new();
    msg_clamp.set_maximum_size(720);
    msg_clamp.set_margin_top(8);
    msg_clamp.set_margin_bottom(8);
    msg_clamp.set_margin_start(12);
    msg_clamp.set_margin_end(12);
    msg_clamp.set_child(Some(&msg_list));

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scrolled.set_child(Some(&msg_clamp));
    outer.append(&scrolled);

    // Auto-scroll to bottom whenever the content height grows (new message added).
    // `notify::upper` fires during the allocation phase; we defer one frame via
    // idle_add_local_once so page_size is fully settled before we set the value.
    scrolled.vadjustment().connect_notify_local(Some("upper"), |adj, _| {
        let adj = adj.clone();
        glib::idle_add_local_once(move || {
            let max = adj.upper() - adj.page_size();
            if max > 0.0 {
                adj.set_value(max);
            }
        });
    });

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
    call_btn.connect_clicked(move |_| sc.input(AppMsg::CallStargazer(pid_c.clone())));
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

    outer.append(&input_row);

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

    (outer, msg_list, call_btn, send_btn, entry)
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
    // Scrolling is handled by the vadjustment `notify::upper` signal wired
    // in build_messages_tab — no manual scroll needed here.
}
