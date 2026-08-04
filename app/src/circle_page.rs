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

/// A small, deterministic accent color per circle (Signal/Matrix-style
/// avatar colors) — derived from the id hash, so it's stable forever and
/// needs no storage of its own. Fixed saturation/lightness, hue from the
/// id's first byte; purely a visual identity touch, no protocol meaning.
pub fn circle_accent_hex(id_hex: &str) -> String {
    let first_byte = hex::decode(id_hex).ok()
        .and_then(|b| b.first().copied())
        .unwrap_or(0);
    let hue = (first_byte as f64 / 255.0) * 360.0;
    let (r, g, b) = hsl_to_rgb(hue, 0.55, 0.55);
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = match h as u32 {
        0..=59   => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _         => (c, 0.0, x),
    };
    (
        ((r1 + m) * 255.0).round() as u8,
        ((g1 + m) * 255.0).round() as u8,
        ((b1 + m) * 255.0).round() as u8,
    )
}

/// A small colored dot rendered via Pango markup — no `CssProvider`
/// plumbing needed for one-off per-widget color.
fn accent_dot(id_hex: &str) -> gtk::Label {
    let dot = gtk::Label::new(None);
    dot.set_markup(&format!(
        "<span foreground='{}'>●</span>", circle_accent_hex(id_hex),
    ));
    dot.set_valign(gtk::Align::Center);
    dot
}

/// One member of a circle, already resolved to something displayable.
pub struct CirclePageMember {
    pub pubkey_hex: String,
    pub label:      String,
    pub access:     String,
    pub is_manage:  bool,
}

/// A known network peer this device could invite (not already a member).
pub struct CirclePagePeer {
    pub peer_id: PeerId,
    pub label:   String,
}

/// Confirm before leaving — reversible in principle (the owner can
/// re-invite) but losing access to future shares isn't obvious from a
/// single click, so it gets the same destructive-confirm treatment as
/// other hard-to-undo actions.
fn present_leave_circle_confirm(
    parent:     &adw::OverlaySplitView,
    sender:     &AsyncComponentSender<AppModel>,
    id_hex:     &str,
    circle_name: &str,
) {
    let dialog = adw::AlertDialog::new(
        Some("Leave Circle?"),
        Some(&format!(
            "You'll stop receiving new shares in \"{circle_name}\". \
             You can be invited back later.",
        )),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("leave", "Leave");
    dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let s = sender.clone();
    let id_hex_owned = id_hex.to_string();
    dialog.connect_response(None, move |_, response| {
        if response == "leave" {
            s.input(AppMsg::LeaveCircle { id_hex: id_hex_owned.clone() });
        }
    });

    dialog.present(Some(parent));
}

/// Build the `adw::ToolbarView` for one circle.
pub fn build_circle_page(
    id_hex:         &str,
    name:           &str,
    members:        &[CirclePageMember],
    known_peers:    &[CirclePagePeer],
    me_pubkey_hex:  &str,
    sender:         &AsyncComponentSender<AppModel>,
    split_view:     &adw::OverlaySplitView,
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

    let title_label = gtk::Label::new(Some(name));
    title_label.add_css_class("title");
    let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    title_box.set_halign(gtk::Align::Center);
    title_box.append(&accent_dot(id_hex));
    title_box.append(&title_label);

    relm4::view! {
        header = adw::HeaderBar {
            set_show_start_title_buttons: false,
            set_show_end_title_buttons: false,

            #[wrap(Some)]
            set_title_widget = &title_box.clone() {},
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
    // Only a manager can revoke someone ELSE's access (the backend enforces
    // this too — see p2panda-auth's `remove()` — but hiding the button
    // avoids a dead-end click for read/write members who can't use it).
    let i_am_manager = members.iter()
        .find(|m| m.pubkey_hex == me_pubkey_hex)
        .is_some_and(|m| m.is_manage);
    for member in members {
        let is_me = member.pubkey_hex == me_pubkey_hex;

        let row = adw::ActionRow::new();
        row.set_title(&if is_me { format!("{} (You)", member.label) } else { member.label.clone() });
        row.set_subtitle(&member.access);
        row.set_activatable(false);

        if is_me {
            // Leaving as the sole manager would orphan the circle (nobody
            // left who could invite/revoke), so that path isn't offered
            // here — only non-owner members get a self-service way out.
            if !member.is_manage {
                let leave_btn = gtk::Button::from_icon_name("system-log-out-symbolic");
                leave_btn.set_tooltip_text(Some("Leave circle"));
                leave_btn.add_css_class("flat");
                leave_btn.add_css_class("circular");
                leave_btn.set_valign(gtk::Align::Center);
                let s = sender.clone();
                let id_hex_owned = id_hex.to_string();
                let name_owned = name.to_string();
                let split_view_owned = split_view.clone();
                leave_btn.connect_clicked(move |_| {
                    present_leave_circle_confirm(&split_view_owned, &s, &id_hex_owned, &name_owned);
                });
                row.add_suffix(&leave_btn);
            }
        } else if i_am_manager {
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
        }
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

            // Two invite buttons: read-only (the common case — "share with
            // this person") and read+write ("invite them to post here too",
            // turning a one-way share into a small collaborative circle —
            // same InterpOp::Author + share_interp_to_circle plumbing
            // either way, just a different Access level at invite time).
            let add_read_btn = gtk::Button::new();
            add_read_btn.set_icon_name("list-add-symbolic");
            add_read_btn.add_css_class("flat");
            add_read_btn.set_valign(gtk::Align::Center);
            add_read_btn.set_tooltip_text(Some("Invite (read only)"));
            let s = sender.clone();
            let id_hex_owned = id_hex.to_string();
            let peer_id = peer.peer_id.clone();
            add_read_btn.connect_clicked(move |_| {
                s.input(AppMsg::InviteToCircle {
                    id_hex: id_hex_owned.clone(), peer_id: peer_id.clone(), can_post: false,
                });
            });
            row.add_suffix(&add_read_btn);

            let add_write_btn = gtk::Button::new();
            add_write_btn.set_icon_name("document-edit-symbolic");
            add_write_btn.add_css_class("flat");
            add_write_btn.set_valign(gtk::Align::Center);
            add_write_btn.set_tooltip_text(Some("Invite with posting rights"));
            let s = sender.clone();
            let id_hex_owned = id_hex.to_string();
            let peer_id = peer.peer_id.clone();
            add_write_btn.connect_clicked(move |_| {
                s.input(AppMsg::InviteToCircle {
                    id_hex: id_hex_owned.clone(), peer_id: peer_id.clone(), can_post: true,
                });
            });
            row.add_suffix(&add_write_btn);
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
