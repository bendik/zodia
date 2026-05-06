//! Factory component for a single interpretation row inside an `AspectView`.
//!
//! Used uniformly for natal aspects, transit aspects, synastry aspects and
//! (in PR2) placement variants — anything keyed by an `InterpKey`.  The row
//! shows a title, an optional glyph suffix, and a body preview pulled from the
//! community store with baseline fallback.  Tapping the row outputs
//! `Activate { key, transit_context }` which the parent `AspectView` translates
//! into a `nav.push(detail_page(...))`.

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};

use zodia_core::InterpKey;

// ── init data ─────────────────────────────────────────────────────────────────

pub struct InterpRowInit {
    pub key:             InterpKey,
    /// Plain-English row title, e.g. "Jupiter trine Venus".
    pub title:           String,
    /// Top suffix line — compact glyph string, e.g. "☽ △ ♀".
    pub symbol_line:     Option<String>,
    /// Bottom suffix line — orb / metadata, stacked under `symbol_line`.
    pub meta_line:       Option<String>,
    /// Optional date-range context for transit aspects, propagated into the
    /// detail page so it can show "Active: 8 Apr – 16 Apr 2026".
    pub transit_context: Option<String>,
    /// Top-body preview for the subtitle.  Empty string → "No interpretation
    /// yet — tap to contribute".
    pub body_preview:    String,
}

impl std::fmt::Debug for InterpRowInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InterpRowInit({})", self.key.to_sig())
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
        key:             InterpKey,
        transit_context: Option<String>,
    },
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct InterpRow {
    pub key:             InterpKey,
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
            key:             init.key,
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
        // Suffix box stacks the symbol line on top of the orb / meta line so the
        // row's title + subtitle have full width to breathe.
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

        // Trailing chevron.
        root.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        // Activation → output.
        {
            let key             = self.key.clone();
            let transit_context = self.transit_context.clone();
            let s               = sender.clone();
            root.connect_activated(move |_| {
                let _ = s.output(InterpRowOut::Activate {
                    key:             key.clone(),
                    transit_context: transit_context.clone(),
                });
            });
        }

        InterpRowWidgets { row: root, symbol_lbl, meta_lbl }
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        let InterpRowMsg::Update(init) = msg;
        self.key             = init.key;
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
