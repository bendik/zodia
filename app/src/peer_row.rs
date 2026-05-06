//! Factory component for a single peer row in the "Others" sidebar section.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};

use zodia_net::PeerId;

use crate::util::sign_glyph;

// ── init data ─────────────────────────────────────────────────────────────────

pub struct PeerRowInit {
    pub peer_id:      PeerId,
    pub solar_month:  u8,
    pub display_name: String,
    pub is_connected: bool,
    pub is_pending:   bool,
    pub dot_filled:   bool,
    pub dot_rgba:     [f32; 4],
    pub unread:       usize,
    pub nickname:     String,
}

impl std::fmt::Debug for PeerRowInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PeerRowInit(···{})", hex::encode_upper(&self.peer_id.0[..4]))
    }
}

// ── messages ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum PeerRowMsg {
    Update(Box<PeerRowInit>),
}

#[derive(Debug)]
pub enum PeerRowOut {
    Activate(PeerId),
    Remove(PeerId),
    SetNickname { peer_id: PeerId, name: String },
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct PeerRow {
    pub peer_id:      PeerId,
    pub solar_month:  u8,
    pub display_name: String,
    pub is_connected: bool,
    pub is_pending:   bool,
    /// Shared with the draw closure so colour changes take effect on queue_draw.
    dot_params:       Rc<Cell<(bool, [f32; 4])>>,
    /// Shared with the GestureClick closure so activation is gated on live state.
    is_connected_cell: Rc<Cell<bool>>,
    pub unread:       usize,
    /// Shared with the nickname dialog closure so the dialog always shows fresh text.
    nickname:         Rc<RefCell<String>>,
}

// ── widgets ───────────────────────────────────────────────────────────────────

pub struct PeerRowWidgets {
    row:        gtk::ListBoxRow,
    dot:        gtk::DrawingArea,
    label:      gtk::Label,
    badge:      gtk::Label,
    edit_img:   gtk::Image,
    remove_btn: gtk::Button,
}

// ── factory component ─────────────────────────────────────────────────────────

impl FactoryComponent for PeerRow {
    type ParentWidget  = gtk::ListBox;
    type Input         = PeerRowMsg;
    type Output        = PeerRowOut;
    type CommandOutput = ();
    type Init          = PeerRowInit;
    type Root          = gtk::ListBoxRow;
    type Widgets       = PeerRowWidgets;
    type Index         = DynamicIndex;

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        PeerRow {
            dot_params:        Rc::new(Cell::new((init.dot_filled, init.dot_rgba))),
            is_connected_cell: Rc::new(Cell::new(init.is_connected)),
            nickname:          Rc::new(RefCell::new(init.nickname)),
            peer_id:      init.peer_id,
            solar_month:  init.solar_month,
            display_name: init.display_name,
            is_connected: init.is_connected,
            is_pending:   init.is_pending,
            unread:       init.unread,
        }
    }

    fn init_root(&self) -> Self::Root {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&hex::encode(&self.peer_id.0));
        row.set_activatable(self.is_connected);
        row
    }

    fn init_widgets(
        &mut self,
        _index: &DynamicIndex,
        root: Self::Root,
        _returned_widget: &gtk::ListBoxRow,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        let peer_hex = hex::encode_upper(&self.peer_id.0[..4]);
        let row = root.clone();

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);

        // ── Presence dot ─────────────────────────────────────────────────────
        let dp = Rc::clone(&self.dot_params);
        let dot = gtk::DrawingArea::new();
        dot.set_size_request(8, 8);
        dot.set_valign(gtk::Align::Center);
        dot.set_draw_func(move |_, cr, w, h| {
            let (filled, rgba) = dp.get();
            let (r, g, b, a) = (
                rgba[0] as f64, rgba[1] as f64,
                rgba[2] as f64, rgba[3] as f64,
            );
            let cx     = w as f64 / 2.0;
            let cy     = h as f64 / 2.0;
            let radius = w.min(h) as f64 / 2.0;
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            cr.set_source_rgba(r, g, b, a);
            if filled {
                let _ = cr.fill();
            } else {
                cr.set_line_width(1.2);
                let _ = cr.stroke();
            }
        });
        hbox.append(&dot);

        // ── Name label ───────────────────────────────────────────────────────
        let glyph = if self.solar_month > 0 { sign_glyph(self.solar_month) } else { "" };
        let label = gtk::Label::new(Some(&format!("{glyph}  {}", self.display_name)));
        label.set_halign(gtk::Align::Start);
        label.set_hexpand(true);
        if self.is_pending { label.add_css_class("dim-label"); }
        hbox.append(&label);

        // ── Unread badge (always present; hidden when zero) ──────────────────
        let badge = gtk::Label::new(None);
        badge.add_css_class("badge");
        badge.add_css_class("accent");
        badge.set_valign(gtk::Align::Center);
        badge.set_visible(self.unread > 0);
        if self.unread > 0 { badge.set_text(&self.unread.to_string()); }
        hbox.append(&badge);

        // ── Edit icon (connected — shown on hover) ───────────────────────────
        let edit_img = gtk::Image::from_icon_name("document-edit-symbolic");
        edit_img.set_pixel_size(16);
        edit_img.set_opacity(0.0);
        edit_img.set_valign(gtk::Align::Center);
        edit_img.set_tooltip_text(Some("Set nickname"));
        edit_img.set_visible(self.is_connected);
        hbox.append(&edit_img);

        // ── Remove button (pending — always visible) ─────────────────────────
        let remove_btn = gtk::Button::from_icon_name("window-close-symbolic");
        remove_btn.add_css_class("flat");
        remove_btn.set_valign(gtk::Align::Center);
        remove_btn.set_tooltip_text(Some("Remove"));
        remove_btn.set_visible(self.is_pending);
        hbox.append(&remove_btn);

        root.set_child(Some(&hbox));

        // ── Activation gesture (connected peers) ─────────────────────────────
        {
            let ic  = Rc::clone(&self.is_connected_cell);
            let pid = self.peer_id.clone();
            let s   = sender.output_sender().clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, n, _, _| {
                if n == 1 && ic.get() {
                    let _ = s.send(PeerRowOut::Activate(pid.clone()));
                }
            });
            row.add_controller(click);
        }

        // ── Remove button signal ──────────────────────────────────────────────
        {
            let pid = self.peer_id.clone();
            let s   = sender.output_sender().clone();
            remove_btn.connect_clicked(move |_| {
                let _ = s.send(PeerRowOut::Remove(pid.clone()));
            });
        }

        // ── Edit (nickname) hover + click ─────────────────────────────────────
        {
            let motion_row = gtk::EventControllerMotion::new();
            let img_enter  = edit_img.clone();
            let img_leave  = edit_img.clone();
            motion_row.connect_enter(move |_, _, _| img_enter.set_opacity(0.4));
            motion_row.connect_leave(move |_| img_leave.set_opacity(0.0));
            row.add_controller(motion_row);

            let motion_img  = gtk::EventControllerMotion::new();
            let img_hover1  = edit_img.clone();
            let img_hover2  = edit_img.clone();
            motion_img.connect_enter(move |_, _, _| img_hover1.set_opacity(1.0));
            motion_img.connect_leave(move |_| img_hover2.set_opacity(0.4));
            edit_img.add_controller(motion_img);

            let pid       = self.peer_id.0;
            let s_out     = sender.output_sender().clone();
            let nick_cell = Rc::clone(&self.nickname);
            let img_ref   = edit_img.clone();
            let click     = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                let current = nick_cell.borrow().clone();
                let dialog  = adw::AlertDialog::new(Some("Set Nickname"), None);
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("set", "Set");
                dialog.set_response_appearance("set", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("set"));
                dialog.set_close_response("cancel");
                let entry = gtk::Entry::new();
                entry.set_text(&current);
                entry.set_placeholder_text(Some("Nickname…"));
                dialog.set_extra_child(Some(&entry));
                let s2 = s_out.clone();
                let e  = entry.clone();
                dialog.connect_response(None, move |_, id| {
                    if id == "set" {
                        let _ = s2.send(PeerRowOut::SetNickname {
                            peer_id: PeerId(pid),
                            name: e.text().to_string(),
                        });
                    }
                });
                dialog.present(Some(&img_ref));
            });
            edit_img.add_controller(click);
        }

        let _ = peer_hex; // suppress unused warning (used only for debug convenience)
        PeerRowWidgets { row, dot, label, badge, edit_img, remove_btn }
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        let PeerRowMsg::Update(init) = msg;
        self.dot_params.set((init.dot_filled, init.dot_rgba));
        self.is_connected_cell.set(init.is_connected);
        *self.nickname.borrow_mut() = init.nickname;
        self.solar_month  = init.solar_month;
        self.display_name = init.display_name;
        self.is_connected = init.is_connected;
        self.is_pending   = init.is_pending;
        self.unread       = init.unread;
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: FactorySender<Self>) {
        // Dot — draw func reads from dot_params Rc<Cell> on each redraw.
        widgets.dot.queue_draw();

        // Label text and dim styling.
        let glyph = if self.solar_month > 0 { sign_glyph(self.solar_month) } else { "" };
        widgets.label.set_text(&format!("{glyph}  {}", self.display_name));
        if self.is_pending {
            widgets.label.add_css_class("dim-label");
        } else {
            widgets.label.remove_css_class("dim-label");
        }

        // Badge.
        widgets.badge.set_visible(self.unread > 0);
        if self.unread > 0 {
            widgets.badge.set_text(&self.unread.to_string());
        }

        // Edit icon / remove button visibility.
        widgets.edit_img.set_visible(self.is_connected);
        widgets.remove_btn.set_visible(self.is_pending);

        // Row activatability.
        widgets.row.set_activatable(self.is_connected);
    }
}
