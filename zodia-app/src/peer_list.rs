//! PeerEntry — a single row in the peer discovery list.
//!
//! Implemented as a `FactoryComponent` so `FactoryVecDeque` can manage the
//! `gtk::ListBox` items reactively.

use gtk::prelude::*;
use relm4::prelude::*;
use zodia_net::PeerId;
use crate::util::sign_glyph;

// ── init / output types ───────────────────────────────────────────────────────

/// Data required to create a `PeerEntry`.
pub struct PeerInit {
    pub peer_id: PeerId,
    pub geohash_prefix: String,
    pub solar_month: u8,
    /// Pre-computed glyph strings, e.g. `["☉△♀", "☉□♄"]`
    pub approximate_aspects: Vec<String>,
}

/// Messages sent from a `PeerEntry` to its parent `AppModel`.
#[derive(Debug)]
pub enum PeerOutput {
    Connect(PeerId),
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct PeerEntry {
    pub peer_id: PeerId,
    aspects_line: String,
    sun_sign: String,
    region: String,
}

// ── factory component ─────────────────────────────────────────────────────────

#[relm4::factory(pub)]
impl FactoryComponent for PeerEntry {
    type Init = PeerInit;
    type Input = ();
    type Output = PeerOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        gtk::ListBoxRow {
            set_selectable: false,
            set_activatable: false,

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 12,
                set_margin_top: 10,
                set_margin_bottom: 10,
                set_margin_start: 14,
                set_margin_end: 14,

                // left: symbol column
                gtk::Label {
                    set_label: "◉",
                    set_css_classes: &["peer-dot"],
                    set_valign: gtk::Align::Center,
                },

                // centre: aspect glyphs + meta
                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_hexpand: true,
                    set_spacing: 2,
                    set_valign: gtk::Align::Center,

                    gtk::Label {
                        set_halign: gtk::Align::Start,
                        set_ellipsize: gtk::pango::EllipsizeMode::End,
                        set_css_classes: &["aspects-line"],
                        #[watch]
                        set_label: &self.aspects_line,
                    },

                    gtk::Label {
                        set_halign: gtk::Align::Start,
                        set_css_classes: &["peer-meta"],
                        #[watch]
                        set_label: &format!("☉ {}  ·  {}", self.sun_sign, self.region),
                    },
                },

                // right: connect button
                gtk::Button {
                    set_label: "Connect",
                    set_valign: gtk::Align::Center,
                    set_css_classes: &["connect-btn"],
                    connect_clicked[sender, peer_id = self.peer_id.clone()] => move |_| {
                        sender.output(PeerOutput::Connect(peer_id.clone())).ok();
                    },
                },
            }
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        let aspects_line = if init.approximate_aspects.is_empty() {
            "— no close aspects detected".to_string()
        } else {
            init.approximate_aspects.join("  ")
        };
        PeerEntry {
            peer_id: init.peer_id,
            aspects_line,
            sun_sign: format!("{} {}", sign_glyph(init.solar_month), {
                const NAMES: [&str; 12] = [
                    "Aries","Taurus","Gemini","Cancer","Leo","Virgo",
                    "Libra","Scorpio","Sagittarius","Capricorn","Aquarius","Pisces",
                ];
                NAMES.get(init.solar_month as usize % 12).copied().unwrap_or("?")
            }),
            region: format!("~{}", init.geohash_prefix),
        }
    }

    fn update(&mut self, _msg: (), _sender: FactorySender<Self>) {}
}
