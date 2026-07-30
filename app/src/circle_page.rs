//! Per-circle navigation page.
//!
//! Added as a named child of the main `gtk::Stack` when a circle's sidebar
//! row is first opened (same pattern as `stargazer_page.rs`'s connected-peer
//! pages) — a plain widget-builder function, not its own relm4 `Component`,
//! since it just needs to fire `AppMsg`s on click, not run its own message
//! loop. Rebuilt from scratch on every open rather than built once and
//! patched in place — circle membership changes are rare enough that this
//! is simpler than tracking per-row widget handles for live updates.

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::AsyncComponentSender;
use zodia_net::PeerId;

use crate::app::{AppModel, AppMsg};

/// One member of a circle, already resolved to something displayable.
pub struct CirclePageMember {
    pub pubkey_hex: String,
    pub label:      String,
    pub access:     String,
}

/// A known network peer this device could invite (not already a member).
pub struct CirclePagePeer {
    pub peer_id: PeerId,
    pub label:   String,
}

/// Build the `adw::ToolbarView` for one circle.
pub fn build_circle_page(
    id_hex:      &str,
    name:        &str,
    members:     &[CirclePageMember],
    known_peers: &[CirclePagePeer],
    sender:      &AsyncComponentSender<AppModel>,
    split_view:  &adw::OverlaySplitView,
) -> adw::ToolbarView {
    relm4::view! {
        sidebar_btn = gtk::Button {
            set_icon_name: "open-menu-symbolic",
            set_tooltip_text: Some("Show sidebar"),
            set_visible: split_view.is_collapsed(),
        }
    }
    {
        let sv = split_view.clone();
        sidebar_btn.connect_clicked(move |_| sv.set_show_sidebar(true));
    }

    relm4::view! {
        header = adw::HeaderBar {
            set_show_start_title_buttons: false,
            set_show_end_title_buttons: false,

            #[wrap(Some)]
            set_title_widget = &gtk::Label {
                set_label: name,
                add_css_class: "title",
            },
        }
    }
    #[cfg(not(target_os = "macos"))]
    header.pack_start(&sidebar_btn);
    #[cfg(target_os = "macos")]
    header.pack_end(&sidebar_btn);

    let root = adw::ToolbarView::new();
    root.add_top_bar(&header);

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 16);

    let members_group = adw::PreferencesGroup::new();
    let n = members.len();
    members_group.set_title("Members");
    members_group.set_description(Some(&format!(
        "{n} member{}", if n == 1 { "" } else { "s" }
    )));
    for member in members {
        let row = adw::ActionRow::new();
        row.set_title(&member.label);
        row.set_subtitle(&member.access);
        row.set_activatable(false);

        let revoke_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        revoke_btn.set_tooltip_text(Some("Revoke access"));
        revoke_btn.add_css_class("flat");
        revoke_btn.add_css_class("circular");
        revoke_btn.set_valign(gtk::Align::Center);
        let s = sender.clone();
        let id_hex_owned = id_hex.to_string();
        let member_hex = member.pubkey_hex.clone();
        revoke_btn.connect_clicked(move |_| {
            s.input(AppMsg::RevokeFromCircle {
                id_hex: id_hex_owned.clone(), member_hex: member_hex.clone(),
            });
        });
        row.add_suffix(&revoke_btn);
        members_group.add(&row);
    }
    content_box.append(&members_group);

    if known_peers.is_empty() {
        let hint = gtk::Label::new(Some(
            "No other discoverable peers to invite right now.",
        ));
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");
        hint.set_margin_top(4);
        content_box.append(&hint);
    } else {
        let invite_group = adw::PreferencesGroup::new();
        invite_group.set_title("Invite");
        for peer in known_peers {
            let row = adw::ActionRow::new();
            row.set_title(&peer.label);
            row.set_activatable(false);

            let add_btn = gtk::Button::new();
            add_btn.set_icon_name("list-add-symbolic");
            add_btn.add_css_class("flat");
            add_btn.set_valign(gtk::Align::Center);
            add_btn.set_tooltip_text(Some("Invite to this circle"));
            let s = sender.clone();
            let id_hex_owned = id_hex.to_string();
            let peer_id = peer.peer_id.clone();
            add_btn.connect_clicked(move |_| {
                s.input(AppMsg::InviteToCircle {
                    id_hex: id_hex_owned.clone(), peer_id: peer_id.clone(),
                });
            });
            row.add_suffix(&add_btn);
            invite_group.add(&row);
        }
        content_box.append(&invite_group);
    }

    relm4::view! {
        scroll = gtk::ScrolledWindow {
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
                set_child = &content_box.clone() {},
            },
        }
    }
    root.set_content(Some(&scroll));

    root
}
