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

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

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
use crate::util::sign_glyph;

/// Build the `adw::ToolbarView` for a connected stargazer.
///
/// Returns `(toolbar_view, msg_list, call_btn, send_btn, entry, typing_label, switcher_title)`.
/// `call_btn` and `send_btn` should be set insensitive when the stargazer is offline.
/// `typing_label` is shown/hidden by the caller when a `TypingIndicatorChanged`
/// event arrives for this peer. `switcher_title` is retained by the caller so
/// the title can be updated when the nickname changes.
#[allow(deprecated)] // ViewSwitcherTitle deprecated in ADW 1.4
pub fn build_stargazer_page(
    peer_id: &PeerId,
    their_blob: &ConsentBlob,
    our_chart: &Chart,
    store: ZodiaStore,
    baseline: Rc<BaselineStore>,
    identity: Rc<IdentityKeypair>,
    sender: &AsyncComponentSender<AppModel>,
    nickname: Option<&str>,
    split_view: &adw::OverlaySplitView,
) -> (adw::ToolbarView, gtk::ListBox, gtk::Button, gtk::Button, gtk::Entry, gtk::Label, adw::ViewSwitcherTitle) {
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
        Some(chart) => crate::aspect_view::launch(crate::aspect_view::AspectViewInit {
            kind:             crate::aspect_view::AspectViewKind::Natal,
            items:            natal_items(&chart.natal_aspects()),
            placements_items: crate::placements::placement_items(chart),
            chart:            None,
            store:            store.clone(),
            baseline:         Rc::clone(&baseline),
            identity:         Rc::clone(&identity),
            parent_sender:    sender.clone(),
        }),
        None => crate::aspect_view::launch(crate::aspect_view::AspectViewInit {
            kind:             crate::aspect_view::AspectViewKind::Synastry,
            items:            vec![],
            placements_items: vec![],
            chart:             None,
            store:            store.clone(),
            baseline:         Rc::clone(&baseline),
            identity:         Rc::clone(&identity),
            parent_sender:    sender.clone(),
        }),
    };
    their_av.set_vexpand(true);

    let their_tab = gtk::Box::new(gtk::Orientation::Vertical, 0);
    their_tab.append(&their_av);
    their_tab.set_vexpand(true);

    let their_page = view_stack.add_titled(&their_tab, Some("their"), "Their Chart");
    their_page.set_icon_name(Some("weather-clear-symbolic"));

    // Synastry tab
    let syn_av = crate::aspect_view::launch(crate::aspect_view::AspectViewInit {
        kind:             crate::aspect_view::AspectViewKind::Synastry,
        items:            synastry_items(&synastry),
        placements_items: vec![],
        chart:            None,
        store:            store.clone(),
        baseline:         Rc::clone(&baseline),
        identity:         Rc::clone(&identity),
        parent_sender:    sender.clone(),
    });
    syn_av.set_vexpand(true);
    let syn_page = view_stack.add_titled(&syn_av, Some("synastry"), "Synastry");
    syn_page.set_icon_name(Some("synastry-symbolic"));

    // Messages tab
    let (messages_widget, msg_list, call_btn, send_btn, entry, typing_label) =
        build_messages_tab(peer_id, sender);
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

    (toolbar_view, msg_list, call_btn, send_btn, entry, typing_label, switcher_title)
}

// ── messages tab ──────────────────────────────────────────────────────────────

/// Build the Messages tab content.
///
/// Returns `(container_widget, msg_list, call_btn, send_btn, entry, typing_label)`.
/// Both action buttons live in the input row so they are always accessible.
fn build_messages_tab(
    peer_id: &PeerId,
    sender: &AsyncComponentSender<AppModel>,
) -> (gtk::Box, gtk::ListBox, gtk::Button, gtk::Button, gtk::Entry, gtk::Label) {
    relm4::view! {
        outer = gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_spacing: 0,
            set_vexpand: true,

            #[name(scrolled)]
            gtk::ScrolledWindow {
                set_vexpand: true,
                set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),

                #[wrap(Some)]
                set_child = &adw::Clamp {
                    set_maximum_size: 720,
                    set_margin_top: 8,
                    set_margin_bottom: 8,
                    set_margin_start: 12,
                    set_margin_end: 12,

                    #[wrap(Some)]
                    #[name(msg_list)]
                    set_child = &gtk::ListBox {
                        set_selection_mode: gtk::SelectionMode::None,
                        add_css_class: "boxed-list",
                        set_vexpand: true,
                    },
                },
            },

            #[name(typing_label)]
            gtk::Label {
                set_text: "Typing…",
                set_halign: gtk::Align::Start,
                set_margin_start: 16,
                add_css_class: "dim-label",
                add_css_class: "caption",
                set_visible: false,
            },

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 8,
                set_margin_start: 12,
                set_margin_end: 12,
                set_margin_top: 4,
                set_margin_bottom: 12,

                #[name(call_btn)]
                gtk::Button {
                    set_icon_name: "call-start-symbolic",
                    add_css_class: "circular",
                    set_tooltip_text: Some("Start voice call"),
                    connect_clicked[sender = sender.clone(), peer_id = peer_id.clone()] => move |_| {
                        sender.input(AppMsg::CallStargazer(peer_id.clone()));
                    },
                },

                #[name(entry)]
                gtk::Entry {
                    set_hexpand: true,
                    set_placeholder_text: Some("Message…"),
                },

                #[name(send_btn)]
                gtk::Button {
                    set_icon_name: "mail-send-symbolic",
                    add_css_class: "suggested-action",
                    add_css_class: "circular",
                    set_tooltip_text: Some("Send"),
                },
            },
        }
    }

    // Auto-scroll to bottom whenever the content height grows (new message added).
    // `notify::upper` fires during allocation; defer one frame via idle so
    // page_size has settled before we set the value.
    scrolled.vadjustment().connect_notify_local(Some("upper"), |adj, _| {
        let adj = adj.clone();
        glib::idle_add_local_once(move || {
            let max = adj.upper() - adj.page_size();
            if max > 0.0 {
                adj.set_value(max);
            }
        });
    });

    // Wire send button + Enter key.  Captured `entry` is the same widget the
    // view! macro bound; cloning it for the closure is just refcount bumps.
    let send = {
        let s       = sender.clone();
        let pid     = peer_id.clone();
        let entry_c = entry.clone();
        move || {
            let text = entry_c.text().trim().to_string();
            if !text.is_empty() {
                s.input(AppMsg::SendChat { peer_id: pid.clone(), text });
                entry_c.set_text("");
            }
        }
    };
    let send_c = send.clone();
    send_btn.connect_clicked(move |_| send_c());
    entry.connect_activate(move |_| send());

    // Debounced typing indicator: the first keystroke after an idle period
    // sends `active: true` immediately; a 3s-idle timer (reset on every
    // further keystroke) sends `active: false`. Clearing the entry (typing
    // it all away, or `send()`'s own `set_text("")`) fires `changed` too and
    // takes the empty-text branch, so a sent message clears the remote's
    // indicator right away rather than waiting out the idle timer.
    {
        let is_typing: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let idle_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let s   = sender.clone();
        let pid = peer_id.clone();
        entry.connect_changed(move |e| {
            if let Some(id) = idle_timer.borrow_mut().take() {
                id.remove();
            }
            if e.text().is_empty() {
                if is_typing.replace(false) {
                    s.input(AppMsg::SetTyping { peer_id: pid.clone(), active: false });
                }
                return;
            }
            if !is_typing.replace(true) {
                s.input(AppMsg::SetTyping { peer_id: pid.clone(), active: true });
            }
            let s2   = s.clone();
            let pid2 = pid.clone();
            let is_typing2 = Rc::clone(&is_typing);
            let idle_timer2 = Rc::clone(&idle_timer);
            let id = glib::timeout_add_local_once(Duration::from_secs(3), move || {
                is_typing2.set(false);
                idle_timer2.borrow_mut().take();
                s2.input(AppMsg::SetTyping { peer_id: pid2.clone(), active: false });
            });
            *idle_timer.borrow_mut() = Some(id);
        });
    }

    (outer, msg_list, call_btn, send_btn, entry, typing_label)
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
