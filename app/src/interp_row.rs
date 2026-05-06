//! Factory component for a single aggregate interpretation row.
//!
//! Each row carries one or more `KeyEntry` items.  Aspects emit one entry; a
//! planet placement emits two (sign + house); an angle placement emits one.
//! Tapping the row outputs `Activate { keys, transit_context }` which the
//! parent `AspectView` forwards to a multi-tab detail page.
//!
//! Subtitle preview is the first non-empty body across the row's keys, so a
//! placement row shows whichever of sign/house has the most content.

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};

use crate::aspect_list::KeyEntry;

// ── init data ─────────────────────────────────────────────────────────────────

pub struct InterpRowInit {
    /// One or more keys for this row's detail tabs.
    pub keys:            Vec<KeyEntry>,
    /// Plain-English row title, e.g. "Jupiter trine Venus" or "Jupiter in Virgo · House 9".
    pub title:           String,
    /// Top suffix line — compact glyph string.
    pub symbol_line:     Option<String>,
    /// Bottom suffix line — orb / metadata, stacked under `symbol_line`.
    pub meta_line:       Option<String>,
    /// Optional date-range context for transit aspects.
    pub transit_context: Option<String>,
    /// Top-body preview for the subtitle.  Empty → "No interpretation yet —
    /// tap to contribute".
    pub body_preview:    String,
}

impl std::fmt::Debug for InterpRowInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sigs: Vec<String> = self.keys.iter().map(|e| e.key.to_sig()).collect();
        write!(f, "InterpRowInit({})", sigs.join(" + "))
    }
}

// ── messages ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)] // PR3 reactive wire reaches for Update; nothing sends it yet
pub enum InterpRowMsg {
    /// Replace all display fields in place — no widget recreation.
    Update(Box<InterpRowInit>),
}

#[derive(Debug)]
pub enum InterpRowOut {
    Activate {
        keys:            Vec<KeyEntry>,
        transit_context: Option<String>,
    },
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct InterpRow {
    pub keys:            Vec<KeyEntry>,
    pub title:           String,
    pub symbol_line:     Option<String>,
    pub meta_line:       Option<String>,
    pub transit_context: Option<String>,
    pub body_preview:    String,
}

pub struct InterpRowWidgets {
    row:        adw::ActionRow,
    symbol_lbl: gtk::Label,
    meta_lbl:   gtk::Label,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}

fn subtitle_for(body_preview: &str) -> String {
    if body_preview.is_empty() {
        "No interpretation yet — tap to contribute".to_string()
    } else {
        truncate(body_preview, 250)
    }
}

// ── factory component ─────────────────────────────────────────────────────────

impl FactoryComponent for InterpRow {
    type ParentWidget  = adw::PreferencesGroup;
    type Input         = InterpRowMsg;
    type Output        = InterpRowOut;
    type CommandOutput = ();
    type Init          = InterpRowInit;
    type Root          = adw::ActionRow;
    type Widgets       = InterpRowWidgets;
    type Index         = DynamicIndex;

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            keys:            init.keys,
            title:           init.title,
            symbol_line:     init.symbol_line,
            meta_line:       init.meta_line,
            transit_context: init.transit_context,
            body_preview:    init.body_preview,
        }
    }

    fn init_root(&self) -> Self::Root {
        let row = adw::ActionRow::new();
        row.set_title(&self.title);
        row.set_subtitle(&subtitle_for(&self.body_preview));
        row.set_subtitle_lines(0);
        row.set_activatable(true);
        row
    }

    fn init_widgets(
        &mut self,
        _index: &DynamicIndex,
        root: Self::Root,
        _returned_widget: &gtk::Widget,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        let suffix_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        suffix_box.set_valign(gtk::Align::Center);

        let symbol_lbl = gtk::Label::new(self.symbol_line.as_deref());
        symbol_lbl.add_css_class("dim-label");
        symbol_lbl.add_css_class("aspect-list");
        symbol_lbl.set_halign(gtk::Align::End);
        symbol_lbl.set_visible(self.symbol_line.is_some());
        suffix_box.append(&symbol_lbl);

        let meta_lbl = gtk::Label::new(self.meta_line.as_deref());
        meta_lbl.add_css_class("dim-label");
        meta_lbl.add_css_class("caption");
        meta_lbl.set_halign(gtk::Align::End);
        meta_lbl.set_visible(self.meta_line.is_some());
        suffix_box.append(&meta_lbl);

        root.add_suffix(&suffix_box);
        root.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        // Activation → output (clones the keys vec each tap; cheap for ≤4 entries).
        {
            let keys            = self.keys.clone();
            let transit_context = self.transit_context.clone();
            let s               = sender.clone();
            root.connect_activated(move |_| {
                let _ = s.output(InterpRowOut::Activate {
                    keys:            keys.clone(),
                    transit_context: transit_context.clone(),
                });
            });
        }

        InterpRowWidgets { row: root, symbol_lbl, meta_lbl }
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        let InterpRowMsg::Update(init) = msg;
        self.keys            = init.keys;
        self.title           = init.title;
        self.symbol_line     = init.symbol_line;
        self.meta_line       = init.meta_line;
        self.transit_context = init.transit_context;
        self.body_preview    = init.body_preview;
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: FactorySender<Self>) {
        widgets.row.set_title(&self.title);
        widgets.row.set_subtitle(&subtitle_for(&self.body_preview));
        match &self.symbol_line {
            Some(s) => { widgets.symbol_lbl.set_text(s); widgets.symbol_lbl.set_visible(true); }
            None    => widgets.symbol_lbl.set_visible(false),
        }
        match &self.meta_line {
            Some(m) => { widgets.meta_lbl.set_text(m); widgets.meta_lbl.set_visible(true); }
            None    => widgets.meta_lbl.set_visible(false),
        }
    }
}
