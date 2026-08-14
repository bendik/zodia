//! Factory component for a single peer row in the "Others" sidebar section.
//!
//! Built with manual `FactoryComponent` impl + `relm4::view!` for the widget
//! tree.  This keeps the declarative tree benefits while leaving room for the
//! cross-widget hover wiring and the nickname-dialog click handler.
//!
//! The row's `widget_name` is set to `"{bucket:02}_{peer_hex}"` so the parent
//! `gtk::ListBox::set_sort_func` can sort rows lexically by bucket then by peer
//! id without needing to look anything up in the model.

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
    /// 0 = connected+online, 1 = incoming pending, 2 = outgoing pending,
    /// 3 = connected+offline.  Lower values float to the top in sort order.
    pub sort_bucket:  u8,
    pub dot_filled:   bool,
    pub dot_rgba:     [f32; 4],
    pub unread:       usize,
    pub nickname:     String,
    pub is_muted:     bool,
    /// At-a-glance synastry tone glyph (✨ / • / ⚡) for connected peers —
    /// `None` while pending, since full synastry needs exact birth data
    /// from an already-exchanged consent. See `zodia_core::synastry_tone`.
    pub compat_glyph:   Option<&'static str>,
    /// Human label for `compat_glyph` (e.g. "Harmonious"), shown as the
    /// row's tooltip.
    pub compat_tooltip: Option<&'static str>,
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
    ToggleMute(PeerId),
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct PeerRow {
    pub peer_id:       PeerId,
    pub solar_month:   u8,
    pub display_name:  String,
    pub is_connected:  bool,
    pub is_pending:    bool,
    pub sort_bucket:   u8,
    pub dot_filled:    bool,
    pub dot_rgba:      [f32; 4],
    pub unread:        usize,
    pub is_muted:      bool,
    pub compat_glyph:   Option<&'static str>,
    pub compat_tooltip: Option<&'static str>,
    /// Live gate read by the activation GestureClick closure.
    is_connected_cell: Rc<Cell<bool>>,
    /// Live nickname read by the dialog when the user opens it.
    nickname:          Rc<RefCell<String>>,
}

// ── widgets ───────────────────────────────────────────────────────────────────

pub struct PeerRowWidgets {
    row:        gtk::ListBoxRow,
    dot:        gtk::DrawingArea,
    label:      gtk::Label,
    badge:      gtk::Label,
    edit_img:   gtk::Image,
    mute_btn:   gtk::Button,
    remove_btn: gtk::Button,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn row_widget_name(bucket: u8, peer_id: &PeerId) -> String {
    format!("{:02}_{}", bucket, hex::encode(&peer_id.0))
}

fn label_text(solar_month: u8, display_name: &str, compat_glyph: Option<&str>) -> String {
    let glyph = if solar_month > 0 { sign_glyph(solar_month) } else { "" };
    match compat_glyph {
        Some(c) => format!("{glyph}  {display_name}  {c}"),
        None => format!("{glyph}  {display_name}"),
    }
}

fn paint_dot(filled: bool, rgba: [f32; 4]) -> impl Fn(&gtk::DrawingArea, &gtk::cairo::Context, i32, i32) + 'static {
    move |_, cr, w, h| {
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
    }
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
        Self {
            is_connected_cell: Rc::new(Cell::new(init.is_connected)),
            nickname:          Rc::new(RefCell::new(init.nickname)),
            peer_id:      init.peer_id,
            solar_month:  init.solar_month,
            display_name: init.display_name,
            is_connected: init.is_connected,
            is_pending:   init.is_pending,
            sort_bucket:  init.sort_bucket,
            dot_filled:   init.dot_filled,
            dot_rgba:     init.dot_rgba,
            unread:       init.unread,
            is_muted:     init.is_muted,
            compat_glyph:   init.compat_glyph,
            compat_tooltip: init.compat_tooltip,
        }
    }

    fn init_root(&self) -> Self::Root {
        relm4::view! {
            row = gtk::ListBoxRow {
                set_widget_name: &row_widget_name(self.sort_bucket, &self.peer_id),
                set_activatable: self.is_connected,
            }
        }
        row
    }

    fn init_widgets(
        &mut self,
        _index: &DynamicIndex,
        root: Self::Root,
        _returned_widget: &gtk::ListBoxRow,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        // Build the inner widget tree declaratively.  Cross-widget signal wiring
        // (motion controllers, dialog click) happens after this block.
        relm4::view! {
            #[local_ref]
            root -> gtk::ListBoxRow {
                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 10,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    set_margin_top: 6,
                    set_margin_bottom: 6,

                    #[name(dot)]
                    gtk::DrawingArea {
                        set_size_request: (8, 8),
                        set_valign: gtk::Align::Center,
                    },

                    #[name(label)]
                    gtk::Label {
                        set_text: &label_text(self.solar_month, &self.display_name, self.compat_glyph),
                        set_halign: gtk::Align::Start,
                        set_hexpand: true,
                        set_tooltip_text: self.compat_tooltip.map(|t| format!("Synastry: {t}")).as_deref(),
                    },

                    #[name(badge)]
                    gtk::Label {
                        add_css_class: "badge",
                        add_css_class: "accent",
                        set_valign: gtk::Align::Center,
                        set_visible: self.unread > 0,
                        set_label: &self.unread.to_string(),
                    },

                    #[name(edit_img)]
                    gtk::Image {
                        set_icon_name: Some("document-edit-symbolic"),
                        set_pixel_size: 16,
                        set_opacity: 0.0,
                        set_valign: gtk::Align::Center,
                        set_tooltip_text: Some("Set nickname"),
                        set_visible: self.is_connected,
                    },

                    #[name(mute_btn)]
                    gtk::Button {
                        set_icon_name: "action-unavailable-symbolic",
                        add_css_class: "flat",
                        add_css_class: "circular",
                        set_valign: gtk::Align::Center,
                        set_visible: self.is_connected,
                        set_opacity: if self.is_muted { 1.0 } else { 0.35 },
                        set_tooltip_text: Some(if self.is_muted { "Unmute" } else { "Mute" }),
                        connect_clicked[sender, peer_id = self.peer_id.clone()] => move |_| {
                            let _ = sender.output(PeerRowOut::ToggleMute(peer_id.clone()));
                        },
                    },

                    #[name(remove_btn)]
                    gtk::Button {
                        set_icon_name: "window-close-symbolic",
                        add_css_class: "flat",
                        set_valign: gtk::Align::Center,
                        set_tooltip_text: Some("Remove"),
                        set_visible: self.is_pending,
                        connect_clicked[sender, peer_id = self.peer_id.clone()] => move |_| {
                            let _ = sender.output(PeerRowOut::Remove(peer_id.clone()));
                        },
                    },
                },
            }
        }

        // Initial dimmed state for pending rows.
        if self.is_pending { label.add_css_class("dimmed"); }

        // Initial dot paint.
        dot.set_draw_func(paint_dot(self.dot_filled, self.dot_rgba));

        // Activation gesture on the row, gated by the live is_connected cell.
        {
            let ic  = Rc::clone(&self.is_connected_cell);
            let pid = self.peer_id.clone();
            let s   = sender.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, n, _, _| {
                if n == 1 && ic.get() {
                    let _ = s.output(PeerRowOut::Activate(pid.clone()));
                }
            });
            root.add_controller(click);
        }

        // Edit-icon hover fade — enter the row reveals 0.4, hover the icon → 1.0.
        {
            let img_a = edit_img.clone();
            let img_b = edit_img.clone();
            let m_row = gtk::EventControllerMotion::new();
            m_row.connect_enter(move |_, _, _| img_a.set_opacity(0.4));
            m_row.connect_leave(move |_| img_b.set_opacity(0.0));
            root.add_controller(m_row);

            let img_c = edit_img.clone();
            let img_d = edit_img.clone();
            let m_img = gtk::EventControllerMotion::new();
            m_img.connect_enter(move |_, _, _| img_c.set_opacity(1.0));
            m_img.connect_leave(move |_| img_d.set_opacity(0.4));
            edit_img.add_controller(m_img);
        }

        // Edit-icon click → nickname dialog.
        {
            let pid       = self.peer_id.0;
            let s         = sender.clone();
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
                let s_inner = s.clone();
                let e       = entry.clone();
                dialog.connect_response(None, move |_, id| {
                    if id == "set" {
                        let _ = s_inner.output(PeerRowOut::SetNickname {
                            peer_id: PeerId(pid),
                            name: e.text().to_string(),
                        });
                    }
                });
                dialog.present(Some(&img_ref));
            });
            edit_img.add_controller(click);
        }

        PeerRowWidgets {
            row: root.clone(),
            dot,
            label,
            badge,
            edit_img,
            mute_btn,
            remove_btn,
        }
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        let PeerRowMsg::Update(init) = msg;
        self.is_connected_cell.set(init.is_connected);
        *self.nickname.borrow_mut() = init.nickname;
        self.solar_month  = init.solar_month;
        self.display_name = init.display_name;
        self.is_connected = init.is_connected;
        self.is_pending   = init.is_pending;
        self.sort_bucket  = init.sort_bucket;
        self.dot_filled   = init.dot_filled;
        self.dot_rgba     = init.dot_rgba;
        self.unread       = init.unread;
        self.is_muted     = init.is_muted;
        self.compat_glyph   = init.compat_glyph;
        self.compat_tooltip = init.compat_tooltip;
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: FactorySender<Self>) {
        // Sort key — `set_sort_func` reads widget_name on each compare.
        widgets.row.set_widget_name(&row_widget_name(self.sort_bucket, &self.peer_id));
        widgets.row.set_activatable(self.is_connected);

        widgets.label.set_text(&label_text(self.solar_month, &self.display_name, self.compat_glyph));
        widgets.label.set_tooltip_text(self.compat_tooltip.map(|t| format!("Synastry: {t}")).as_deref());
        if self.is_pending || self.is_muted {
            widgets.label.add_css_class("dimmed");
        } else {
            widgets.label.remove_css_class("dimmed");
        }

        widgets.badge.set_visible(self.unread > 0);
        if self.unread > 0 {
            widgets.badge.set_label(&self.unread.to_string());
        }

        widgets.edit_img.set_visible(self.is_connected);
        widgets.mute_btn.set_visible(self.is_connected);
        widgets.mute_btn.set_opacity(if self.is_muted { 1.0 } else { 0.35 });
        widgets.mute_btn.set_tooltip_text(Some(if self.is_muted { "Unmute" } else { "Mute" }));
        widgets.remove_btn.set_visible(self.is_pending);

        // Re-set draw func so the closure captures fresh colour state.
        widgets.dot.set_draw_func(paint_dot(self.dot_filled, self.dot_rgba));
        widgets.dot.queue_draw();
    }
}
