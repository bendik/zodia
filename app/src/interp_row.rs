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
    /// Optional compact glyph string shown as a suffix, e.g. "☽△♀  orb 2.3°".
    pub glyph_suffix:    Option<String>,
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
    pub glyph_suffix:    Option<String>,
    pub transit_context: Option<String>,
    pub body_preview:    String,
}

pub struct InterpRowWidgets {
    row:       adw::ActionRow,
    glyph_lbl: gtk::Label,
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
            glyph_suffix:    init.glyph_suffix,
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
        // Glyph suffix label (always present; hidden when no glyph).
        let glyph_lbl = gtk::Label::new(self.glyph_suffix.as_deref());
        glyph_lbl.add_css_class("dim-label");
        glyph_lbl.add_css_class("caption");
        glyph_lbl.add_css_class("aspect-list");
        glyph_lbl.set_visible(self.glyph_suffix.is_some());
        root.add_suffix(&glyph_lbl);

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

        InterpRowWidgets { row: root, glyph_lbl }
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        let InterpRowMsg::Update(init) = msg;
        self.key             = init.key;
        self.title           = init.title;
        self.glyph_suffix    = init.glyph_suffix;
        self.transit_context = init.transit_context;
        self.body_preview    = init.body_preview;
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: FactorySender<Self>) {
        widgets.row.set_title(&self.title);
        widgets.row.set_subtitle(&subtitle_for(&self.body_preview));
        match &self.glyph_suffix {
            Some(g) => {
                widgets.glyph_lbl.set_text(g);
                widgets.glyph_lbl.set_visible(true);
            }
            None => widgets.glyph_lbl.set_visible(false),
        }
    }
}
