//! `FeedView` — reusable activity-feed widget.
//!
//! Drives a `FactoryVecDeque<FeedCard>` of one card per `FeedItem`.  The
//! Sky tab uses an unfiltered view; per-aspect pages use the same widget
//! with `interp_key` filtering.
//!
//! # Phase E scope
//!
//! - Variant-specific cards: authored / affirm / response / transit-enter / transit-leave.
//! - Relative timestamps with absolute on hover.
//! - Read/unread accent dot.
//! - Click → emits `FeedViewOut::Activate` so the host can navigate.
//! - Manual "mark read" via the click; the auto-mark-read-on-dwell pattern
//!   from the PRD is deferred to a follow-up — the click affordance is
//!   enough to validate the read-state plumbing end-to-end.

use libadwaita;
use libadwaita::gtk;
use libadwaita::gtk::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::prelude::*;
use relm4::Component;

use crate::feed_item::{FeedItem, FeedPayload};

// ── public entry point ────────────────────────────────────────────────────────

pub struct FeedViewInit {
    /// Per-key filter for per-aspect pages.  `None` shows every item.
    pub filter_key: Option<String>,
    /// Initial items to backfill the feed with on mount.  Caller usually
    /// supplies recent items from `ZodiaStore::recent_feed_rows`.
    pub initial:    Vec<FeedItem>,
    /// Local identity's verifying key.  Cards whose author matches show a
    /// "Revoke" affordance; non-self cards don't.
    pub me:         [u8; 32],
}

#[derive(Debug)]
pub enum FeedViewMsg {
    /// New item arrived from the live stream.  Inserts in correct timestamp
    /// position; duplicates (by `event_id`) are dropped.
    Push(FeedItem),
    /// Replace the whole list — used on refilter or full refresh.
    Reset(Vec<FeedItem>),
    /// Mark `event_id` as read in the UI (parent owns persistence).
    MarkRead([u8; 32]),
    /// Change the active category filter; rebuilds the visible list from
    /// the full backlog (kept in `all_items`).
    SetCategory(FeedCategory),
    /// Internal: a card was activated.  Relayed to output as `Activate`.
    #[doc(hidden)]
    CardActivated { event_id: [u8; 32], payload: FeedPayload },
    /// Internal: user pressed revoke on a card.  Relayed to output.
    #[doc(hidden)]
    CardRevoke { log_id: [u8; 32] },
    /// Internal: user pressed "share to circle" on a card.  Relayed to output.
    #[doc(hidden)]
    CardShareToCircle { interp_key: String, body: String },
    /// Internal: explicit read-state change from card context menu.
    #[doc(hidden)]
    CardSetRead { event_id: [u8; 32], read: bool },
}

/// Top-level filter applied alongside `filter_key`.  Personal = sky events
/// affecting the local chart (transits, house transits).  Sky = global
/// transiting-body aspects.  Community = doc edits, affirms, vetoes,
/// presence chips, and legacy authored interpretations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedCategory {
    All,
    Personal,
    Sky,
    Community,
}

#[derive(Debug)]
pub enum FeedViewOut {
    /// Card was activated; the host should navigate / mark read in the store.
    Activate { event_id: [u8; 32], payload: ActivatedPayload },
    /// User asked to revoke + delete their own authored interpretation.
    /// `log_id` = BLAKE3(interp_key || body) — the row primary key.
    Revoke { log_id: [u8; 32] },
    /// User asked to share their own authored interpretation privately
    /// with a circle. Host opens the circle picker (it owns circle state,
    /// this component doesn't).
    ShareToCircle { interp_key: String, body: String },
    /// Explicit read-state change from card context menu.  Host persists to
    /// `feed_read` and refreshes the bell badge.
    SetRead { event_id: [u8; 32], read: bool },
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // log_id fields reserved for future "scroll to op"
pub enum ActivatedPayload {
    OpenInterpKey(String),
    OpenAffirmTarget { target_log_id: [u8; 32], target_key: String },
    OpenResponseParent { parent_log_id: [u8; 32], interp_key: String },
    OpenTransitKey(String),
}

/// Spawn the component; returns its root widget + an input sender.  Output
/// forwards through `parent_input`/`map` per relm4 idiom.
pub fn launch<F, M>(
    init:         FeedViewInit,
    parent_input: &relm4::Sender<M>,
    map:          F,
) -> (gtk::ScrolledWindow, relm4::Sender<FeedViewMsg>)
where
    F: Fn(FeedViewOut) -> M + Send + 'static,
    M: Send + 'static,
{
    use relm4::ComponentController;
    let mut ctl = <FeedView as Component>::builder()
        .launch(init)
        .forward(parent_input, map);
    let widget = ctl.widget().clone();
    let sender = ctl.sender().clone();
    ctl.detach_runtime();
    (widget, sender)
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct FeedView {
    filter_key: Option<String>,
    category:   FeedCategory,
    me:         [u8; 32],
    cards:      FactoryVecDeque<FeedCard>,
    /// Dedup index over current event_ids.  Cheap to maintain; the factory
    /// itself doesn't provide a key-lookup so we mirror the set here.
    known:      std::collections::HashSet<[u8; 32]>,
    /// Full backlog (unfiltered apart from `filter_key`).  Kept so changing
    /// `category` can rebuild the visible list without re-fetching from the
    /// store.  Newest-first ordering matches the rendered list.
    all_items:  Vec<FeedItem>,
}

/// Continuous viewport-visible time required before a card auto-marks read.
/// PRD §"FeedView" specified 1.5 s — short enough to feel responsive,
/// long enough to skip "scrolled past" cards.
const DWELL_AUTO_READ_MS: u64 = 1_500;

/// Scan cadence for the dwell timer.  Half the dwell threshold so we
/// detect crossings reliably; 750 ms keeps idle wakeups cheap.
const DWELL_SCAN_MS: u32 = 750;

#[allow(dead_code)]
pub struct FeedViewWidgets {
    empty_label: gtk::Label,
    filter_bar:  gtk::Box,
    btn_all:       gtk::ToggleButton,
    btn_personal:  gtk::ToggleButton,
    btn_sky:       gtk::ToggleButton,
    btn_community: gtk::ToggleButton,
}

impl SimpleComponent for FeedView {
    type Init    = FeedViewInit;
    type Input   = FeedViewMsg;
    type Output  = FeedViewOut;
    type Root    = gtk::ScrolledWindow;
    type Widgets = FeedViewWidgets;

    fn init_root() -> Self::Root {
        gtk::ScrolledWindow::new()
    }

    fn init(
        init:   Self::Init,
        root:   Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");

        // Same Clamp pattern aspect_view uses so Sky's max content width
        // matches the rest of the app rather than going edge-to-edge.
        let clamp = libadwaita::Clamp::new();
        clamp.set_maximum_size(720);
        clamp.set_margin_top(8);
        clamp.set_margin_bottom(8);
        clamp.set_margin_start(12);
        clamp.set_margin_end(12);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);

        // Today's Moon phase — ambient, always-relevant astrological
        // context that needs no natal chart at all, shown at the top of
        // the live activity feed. Computed once at page-build time; the
        // phase only changes over ~3.5 days so a live ticker isn't needed.
        if let Ok(phase) = zodia_core::moon_phase_at(zodia_core::current_jdn()) {
            let moon_label = gtk::Label::new(Some(&format!(
                "Today: {} {}", phase.symbol(), phase.name(),
            )));
            moon_label.add_css_class("dim-label");
            moon_label.add_css_class("caption");
            moon_label.set_halign(gtk::Align::Center);
            moon_label.set_margin_top(4);
            container.append(&moon_label);
        }

        // Category filter bar — segmented ToggleButtons (linked style).
        let filter_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        filter_bar.add_css_class("linked");
        filter_bar.set_halign(gtk::Align::Center);
        filter_bar.set_margin_top(4);
        filter_bar.set_margin_bottom(4);
        let btn_all       = labeled_toggle("☰",  "All");
        let btn_personal  = labeled_toggle("◐", "Personal");
        let btn_sky       = labeled_toggle("✦", "Sky");
        let btn_community = labeled_toggle("✎", "Community");
        // Group: link via `set_group` so only one is active at a time.
        btn_personal.set_group(Some(&btn_all));
        btn_sky.set_group(Some(&btn_all));
        btn_community.set_group(Some(&btn_all));
        btn_all.set_active(true);
        filter_bar.append(&btn_all);
        filter_bar.append(&btn_personal);
        filter_bar.append(&btn_sky);
        filter_bar.append(&btn_community);

        // Wire each button to dispatch SetCategory on toggle-on.
        let s = sender.input_sender().clone();
        btn_all.connect_toggled(move |b| {
            if b.is_active() { let _ = s.send(FeedViewMsg::SetCategory(FeedCategory::All)); }
        });
        let s = sender.input_sender().clone();
        btn_personal.connect_toggled(move |b| {
            if b.is_active() { let _ = s.send(FeedViewMsg::SetCategory(FeedCategory::Personal)); }
        });
        let s = sender.input_sender().clone();
        btn_sky.connect_toggled(move |b| {
            if b.is_active() { let _ = s.send(FeedViewMsg::SetCategory(FeedCategory::Sky)); }
        });
        let s = sender.input_sender().clone();
        btn_community.connect_toggled(move |b| {
            if b.is_active() { let _ = s.send(FeedViewMsg::SetCategory(FeedCategory::Community)); }
        });

        container.append(&filter_bar);
        container.append(&list);
        let empty_label = gtk::Label::new(Some("No activity yet"));
        empty_label.add_css_class("dim-label");
        empty_label.set_halign(gtk::Align::Center);
        empty_label.set_margin_top(24);
        container.append(&empty_label);

        clamp.set_child(Some(&container));
        root.set_child(Some(&clamp));
        root.set_hscrollbar_policy(gtk::PolicyType::Never);
        root.set_vexpand(true);

        let cards: FactoryVecDeque<FeedCard> = FactoryVecDeque::builder()
            .launch(list.clone())
            .forward(sender.input_sender(), |out| match out {
                FeedCardOut::Activated { event_id, payload } =>
                    FeedViewMsg::CardActivated { event_id, payload },
                FeedCardOut::Revoke { log_id } =>
                    FeedViewMsg::CardRevoke { log_id },
                FeedCardOut::ShareToCircle { interp_key, body } =>
                    FeedViewMsg::CardShareToCircle { interp_key, body },
                FeedCardOut::SetRead { event_id, read } =>
                    FeedViewMsg::CardSetRead { event_id, read },
            });

        let mut model = FeedView {
            filter_key: init.filter_key,
            category:   FeedCategory::All,
            me:         init.me,
            cards,
            known:      std::collections::HashSet::new(),
            all_items:  Vec::new(),
        };
        model.reset(init.initial);

        // Dwell auto-read timer.  Runs on the main loop; checks viewport
        // overlap for each card and emits CardSetRead { read: true } for
        // cards whose continuous viewport dwell crosses the threshold.
        spawn_dwell_timer(
            root.clone(),
            list.clone(),
            sender.input_sender().clone(),
        );

        let widgets = FeedViewWidgets {
            empty_label, filter_bar,
            btn_all, btn_personal, btn_sky, btn_community,
        };
        widgets.empty_label.set_visible(model.cards.is_empty());

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            FeedViewMsg::Push(item)   => self.push(item),
            FeedViewMsg::Reset(items) => self.reset(items),
            FeedViewMsg::MarkRead(id) => self.set_read(id, true),
            FeedViewMsg::SetCategory(cat) => {
                self.category = cat;
                let backlog = self.all_items.clone();
                self.reset(backlog);
            }
            FeedViewMsg::CardRevoke { log_id } => {
                let _ = sender.output(FeedViewOut::Revoke { log_id });
            }
            FeedViewMsg::CardShareToCircle { interp_key, body } => {
                let _ = sender.output(FeedViewOut::ShareToCircle { interp_key, body });
            }
            FeedViewMsg::CardSetRead { event_id, read } => {
                self.set_read(event_id, read);
                let _ = sender.output(FeedViewOut::SetRead { event_id, read });
            }
            FeedViewMsg::CardActivated { event_id, payload } => {
                // Mark the card visually read locally and forward as output.
                self.set_read(event_id, true);
                let out = match payload {
                    FeedPayload::InterpAuthored { interp_key, .. } =>
                        ActivatedPayload::OpenInterpKey(interp_key),
                    FeedPayload::AffirmAdded { target_log_id, target_key, .. } =>
                        ActivatedPayload::OpenAffirmTarget { target_log_id, target_key },
                    FeedPayload::ResponseAdded { parent_log_id, interp_key, .. } =>
                        ActivatedPayload::OpenResponseParent { parent_log_id, interp_key },
                    FeedPayload::TransitActive { transit_key, .. } =>
                        ActivatedPayload::OpenTransitKey(transit_key),
                    FeedPayload::SkyAspectActive { aspect_key, .. } =>
                        ActivatedPayload::OpenTransitKey(aspect_key),
                    FeedPayload::HouseTransitActive { transit_key, .. } =>
                        ActivatedPayload::OpenTransitKey(transit_key),
                    FeedPayload::DocEdited                  { interp_key, .. } => ActivatedPayload::OpenInterpKey(interp_key),
                    FeedPayload::BlockYouAuthoredWasEdited  { interp_key, .. } => ActivatedPayload::OpenInterpKey(interp_key),
                    FeedPayload::DocRolledBack              { interp_key, .. } => ActivatedPayload::OpenInterpKey(interp_key),
                    FeedPayload::DocAffirmed                { interp_key, .. } => ActivatedPayload::OpenInterpKey(interp_key),
                    FeedPayload::EditorPresenceChanged      { interp_key, .. } => ActivatedPayload::OpenInterpKey(interp_key),
                };
                let _ = sender.output(FeedViewOut::Activate { event_id, payload: out });
            }
        }
    }


    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        widgets.empty_label.set_visible(self.cards.is_empty());
    }
}

impl FeedView {
    fn push(&mut self, item: FeedItem) {
        // Maintain the full backlog regardless of active category so a
        // later category switch can rebuild without re-fetching.
        if !match_key(&self.filter_key, &item) { return; }
        if self.all_items.iter().any(|i| i.event_id == item.event_id) {
            // Backlog already has it; visible-list dedup handled below.
        } else {
            // Insert newest-first into all_items.
            let mut at = self.all_items.len();
            for (i, existing) in self.all_items.iter().enumerate() {
                if existing.timestamp < item.timestamp { at = i; break; }
            }
            self.all_items.insert(at, item.clone());
        }
        if !match_category(self.category, &item) { return; }
        if !self.known.insert(item.event_id) { return; }
        let mut g = self.cards.guard();
        let mut insert_at = g.len();
        for i in 0..g.len() {
            if let Some(card) = g.get(i) {
                if card.item.timestamp < item.timestamp {
                    insert_at = i;
                    break;
                }
            }
        }
        g.insert(insert_at, FeedCardInit { item, me: self.me });
    }

    fn reset(&mut self, items: Vec<FeedItem>) {
        let filter_key = self.filter_key.clone();
        let cat = self.category;
        // Backlog := every item passing the per-key filter, newest-first.
        let mut backlog: Vec<FeedItem> = items.into_iter()
            .filter(|it| match_key(&filter_key, it))
            .collect();
        backlog.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        self.all_items = backlog.clone();

        // Visible := backlog further filtered by category.
        let visible: Vec<FeedItem> = backlog.into_iter()
            .filter(|it| match_category(cat, it))
            .collect();

        self.known.clear();
        let mut g = self.cards.guard();
        g.clear();
        for it in visible {
            if self.known.insert(it.event_id) {
                g.push_back(FeedCardInit { item: it, me: self.me });
            }
        }
    }

    fn set_read(&mut self, event_id: [u8; 32], read: bool) {
        let g = self.cards.guard();
        for i in 0..g.len() {
            if let Some(card) = g.get(i) {
                if card.item.event_id == event_id {
                    g.send(i, FeedCardMsg::SetRead(read));
                    break;
                }
            }
        }
    }

}

fn match_key(filter_key: &Option<String>, item: &FeedItem) -> bool {
    match (filter_key, item.interp_key()) {
        (None, _)               => true,
        (Some(want), Some(got)) => want == got,
        (Some(_),    None)      => false,
    }
}

fn match_category(cat: FeedCategory, item: &FeedItem) -> bool {
    use FeedPayload::*;
    match cat {
        FeedCategory::All       => true,
        FeedCategory::Personal  => matches!(
            &item.payload,
            TransitActive { .. } | HouseTransitActive { .. }
        ),
        FeedCategory::Sky       => matches!(
            &item.payload,
            SkyAspectActive { .. }
        ),
        FeedCategory::Community => matches!(
            &item.payload,
            InterpAuthored { .. } | AffirmAdded { .. } | ResponseAdded { .. }
            | DocEdited { .. } | BlockYouAuthoredWasEdited { .. }
            | DocRolledBack { .. } | DocAffirmed { .. }
            | EditorPresenceChanged { .. }
        ),
    }
}

/// Spawn the per-FeedView dwell timer.  Walks visible `ListBoxRow` children
/// on each tick, computes viewport overlap, and emits `CardSetRead` for
/// rows whose continuous dwell crosses `DWELL_AUTO_READ_MS`.  Per-card
/// dwell start times live in `dwell_started` keyed by `event_id` — we
/// store the event_id in each row via `set_widget_name` so the timer can
/// look it up without owning the FeedCard models.
fn spawn_dwell_timer(
    scroll: gtk::ScrolledWindow,
    list:   gtk::ListBox,
    input:  relm4::Sender<FeedViewMsg>,
) {
    let dwell: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, u64>>> =
        std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
    libadwaita::glib::timeout_add_local(
        std::time::Duration::from_millis(DWELL_SCAN_MS as u64),
        move || {
            // Stop ticking if widgets were dropped.
            if scroll.parent().is_none() && !scroll.is_visible() {
                return libadwaita::glib::ControlFlow::Continue;
            }
            let vadj = scroll.vadjustment();
            let viewport_top = vadj.value();
            let viewport_bot = viewport_top + vadj.page_size();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mut still_visible: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut row_opt = list.first_child();
            while let Some(row) = row_opt {
                let next = row.next_sibling();
                let name = row.widget_name().to_string();
                if !name.is_empty() {
                    // `compute_bounds(target)` returns the row's rect in the
                    // target's coordinate space — pass the list so we get
                    // the unscrolled Y, then compare against vadjustment.
                    let bounds = row.compute_bounds(&list);
                    let (row_top, row_bot) = match bounds {
                        Some(r) => (r.y() as f64, (r.y() + r.height()) as f64),
                        None    => { row_opt = next; continue; }
                    };
                    let in_view = row_bot > viewport_top && row_top < viewport_bot;
                    if in_view {
                        still_visible.insert(name.clone());
                        let mut map = dwell.borrow_mut();
                        let started = *map.entry(name.clone()).or_insert(now_ms);
                        if now_ms.saturating_sub(started) >= DWELL_AUTO_READ_MS {
                            if let Some(event_id) = decode_event_id(&name) {
                                let _ = input.send(FeedViewMsg::CardSetRead {
                                    event_id, read: true,
                                });
                                // Remove so we don't re-emit every tick.
                                map.remove(&name);
                            }
                        }
                    }
                }
                row_opt = next;
            }
            // Drop dwell entries that left the viewport so re-entering
            // restarts the timer (matches the PRD "intersection observer"
            // shape: dwell is reset when the row leaves view).
            dwell.borrow_mut().retain(|k, _| still_visible.contains(k));
            libadwaita::glib::ControlFlow::Continue
        },
    );
}

/// Decode a 32-byte event_id from the row's hex `widget_name`.  Returns
/// `None` for non-card rows (filter bar children etc.) — they have no
/// `widget_name` matching this format.
fn decode_event_id(name: &str) -> Option<[u8; 32]> {
    if name.len() != 64 { return None; }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&name[i*2..i*2+2], 16).ok()?;
    }
    Some(out)
}

/// Build a `ToggleButton` with a glyph + label so the icon-only widget
/// remains decipherable.  Used by the segmented filter bar.
fn labeled_toggle(glyph: &str, label: &str) -> gtk::ToggleButton {
    let btn = gtk::ToggleButton::new();
    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let g = gtk::Label::new(Some(glyph));
    let l = gtk::Label::new(Some(label));
    inner.append(&g);
    inner.append(&l);
    btn.set_child(Some(&inner));
    btn
}

// ── card factory ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct FeedCardInit {
    pub item: FeedItem,
    pub me:   [u8; 32],
}

pub struct FeedCard {
    pub item: FeedItem,
    pub me:   [u8; 32],
    pub read: bool,
}

#[derive(Debug)]
pub enum FeedCardMsg {
    SetRead(bool),
}

#[derive(Debug)]
pub enum FeedCardOut {
    Activated { event_id: [u8; 32], payload: FeedPayload },
    Revoke    { log_id:   [u8; 32] },
    ShareToCircle { interp_key: String, body: String },
    SetRead   { event_id: [u8; 32], read: bool },
}

/// Compute the row's log_id (BLAKE3 content-hash) from an authored payload.
/// `None` for non-authored variants.  Used to derive the revoke target
/// since `event_id` may be the op-hash (different from log_id) for
/// pipeline-sourced events.
fn authored_log_id(payload: &FeedPayload) -> Option<[u8; 32]> {
    if let FeedPayload::InterpAuthored { interp_key, body, .. } = payload {
        let mut h = blake3::Hasher::new();
        h.update(interp_key.as_bytes());
        h.update(body.as_bytes());
        Some(*h.finalize().as_bytes())
    } else {
        None
    }
}

/// True iff `payload` is an InterpAuthored whose author matches `me`.
fn is_own_authored(payload: &FeedPayload, me: &[u8; 32]) -> bool {
    matches!(payload, FeedPayload::InterpAuthored { author, .. } if author.as_bytes() == me)
}

/// True iff `payload` is content of ours that can be shared to a circle —
/// either a legacy authored interpretation or a collaborative doc edit we
/// made ourselves. `DocEdited` carries no body text (unlike
/// `InterpAuthored`), so the share handler sends an empty body for it and
/// `AppMsg::OpenShareToCirclePicker` loads the doc's current text instead.
fn can_share(payload: &FeedPayload, me: &[u8; 32]) -> bool {
    is_own_authored(payload, me)
        || matches!(payload, FeedPayload::DocEdited { editor, .. } if editor.as_bytes() == me)
}

pub struct FeedCardWidgets {
    glyph:      gtk::Label,
    title:      gtk::Label,
    subtitle:   gtk::Label,
    footer:     gtk::Label,
    sub_meta:   gtk::Label,
    unread:     gtk::Image,
    revoke_btn: gtk::Button,
    share_btn:  gtk::Button,
}

impl FactoryComponent for FeedCard {
    type Init           = FeedCardInit;
    type Input          = FeedCardMsg;
    type Output         = FeedCardOut;
    type CommandOutput  = ();
    type ParentWidget   = gtk::ListBox;
    type Widgets        = FeedCardWidgets;
    type Root           = gtk::ListBoxRow;
    type Index          = DynamicIndex;

    fn init_root(&self) -> Self::Root {
        let row = gtk::ListBoxRow::new();
        // Tag with the event_id hex so the dwell scanner can identify the
        // card without holding the FeedCard model directly.
        row.set_widget_name(&hex::encode(self.item.event_id));
        row
    }

    fn init_model(
        init: Self::Init,
        _index: &Self::Index,
        _sender: FactorySender<Self>,
    ) -> Self {
        Self { item: init.item, me: init.me, read: false }
    }

    fn init_widgets(
        &mut self,
        _index: &Self::Index,
        root: Self::Root,
        _returned_widget: &<Self::ParentWidget as relm4::factory::FactoryView>::ReturnedWidget,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        root.set_activatable(true);

        root.set_activatable(true);

        let outer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        outer.set_margin_top(10);
        outer.set_margin_bottom(10);
        outer.set_margin_start(12);
        outer.set_margin_end(12);

        // Left: glyph column.
        let glyph = gtk::Label::new(None);
        glyph.set_valign(gtk::Align::Start);
        glyph.set_margin_top(2);
        glyph.add_css_class("title-4");
        outer.append(&glyph);

        // Middle: title + subtitle.  hexpand fills available width.
        let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text_box.set_hexpand(true);

        let title = gtk::Label::new(None);
        title.set_halign(gtk::Align::Start);
        title.set_wrap(true);
        title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        title.add_css_class("title-4");

        let subtitle = gtk::Label::new(None);
        subtitle.set_halign(gtk::Align::Start);
        subtitle.set_wrap(true);
        subtitle.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        subtitle.add_css_class("caption");
        subtitle.add_css_class("dim-label");

        text_box.append(&title);
        text_box.append(&subtitle);
        outer.append(&text_box);

        // Right: two stacked right-aligned labels — date range on top,
        // orb / pubkey detail below.  Mirrors the old chart-list row.
        let right_col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        right_col.set_valign(gtk::Align::Start);
        right_col.set_margin_top(2);
        let footer = gtk::Label::new(None);
        footer.set_halign(gtk::Align::End);
        footer.add_css_class("caption");
        footer.add_css_class("dim-label");
        let sub_meta = gtk::Label::new(None);
        sub_meta.set_halign(gtk::Align::End);
        sub_meta.add_css_class("caption");
        sub_meta.add_css_class("dim-label");
        right_col.append(&footer);
        right_col.append(&sub_meta);
        outer.append(&right_col);

        // Right: share-to-circle button.
        let share_btn = gtk::Button::from_icon_name("avatar-default-symbolic");
        share_btn.set_tooltip_text(Some("Share to a circle"));
        share_btn.add_css_class("flat");
        share_btn.add_css_class("circular");
        share_btn.set_valign(gtk::Align::Center);
        share_btn.set_visible(can_share(&self.item.payload, &self.me));
        outer.append(&share_btn);

        // Right: revoke button.
        let revoke_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        revoke_btn.set_tooltip_text(Some("Revoke and delete"));
        revoke_btn.add_css_class("flat");
        revoke_btn.add_css_class("circular");
        revoke_btn.set_valign(gtk::Align::Center);
        revoke_btn.set_visible(is_own_authored(&self.item.payload, &self.me));
        outer.append(&revoke_btn);

        let unread = gtk::Image::new();
        root.set_child(Some(&outer));

        render_into(&self.item, &glyph, &title, &subtitle, &footer, &sub_meta);

        // Click on the row body → emit Activated.  GestureClick is the
        // right primitive here: `connect_activate` on ListBoxRow only
        // fires for keyboard Enter/Space, not pointer clicks.
        let item_clone = self.item.clone();
        let s = sender.clone();
        let click = gtk::GestureClick::new();
        click.connect_released(move |gesture, _n, _x, _y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let _ = s.output(FeedCardOut::Activated {
                event_id: item_clone.event_id,
                payload:  item_clone.payload.clone(),
            });
        });
        root.add_controller(click);

        // Right-click + long-press context menu: Mark read / Mark unread.
        // The Popover is owned by the row so it survives between activations.
        let popover = gtk::Popover::new();
        popover.set_has_arrow(true);
        popover.set_parent(&root);
        let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mark_unread = gtk::Button::with_label("Mark as unread");
        mark_unread.add_css_class("flat");
        mark_unread.set_halign(gtk::Align::Fill);
        let mark_read = gtk::Button::with_label("Mark as read");
        mark_read.add_css_class("flat");
        mark_read.set_halign(gtk::Align::Fill);
        menu_box.append(&mark_unread);
        menu_box.append(&mark_read);
        popover.set_child(Some(&menu_box));

        let event_id_for_menu = self.item.event_id;
        let s_unread = sender.clone();
        let pop_close = popover.clone();
        mark_unread.connect_clicked(move |_| {
            pop_close.popdown();
            let _ = s_unread.output(FeedCardOut::SetRead {
                event_id: event_id_for_menu, read: false,
            });
        });
        let s_read = sender.clone();
        let pop_close = popover.clone();
        mark_read.connect_clicked(move |_| {
            pop_close.popdown();
            let _ = s_read.output(FeedCardOut::SetRead {
                event_id: event_id_for_menu, read: true,
            });
        });

        // Secondary mouse button (right-click) → popup at pointer.
        let pop_secondary = popover.clone();
        let secondary = gtk::GestureClick::new();
        secondary.set_button(3);
        secondary.connect_pressed(move |g, _n, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            pop_secondary.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                x as i32, y as i32, 1, 1,
            )));
            pop_secondary.popup();
        });
        root.add_controller(secondary);

        // Long-press (touch) → same popup.
        let pop_long = popover.clone();
        let long = gtk::GestureLongPress::new();
        long.connect_pressed(move |g, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            pop_long.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                x as i32, y as i32, 1, 1,
            )));
            pop_long.popup();
        });
        root.add_controller(long);

        // Revoke button → emit Revoke with the computed log_id.
        let revoke_payload = self.item.payload.clone();
        let s2 = sender.clone();
        revoke_btn.connect_clicked(move |_| {
            if let Some(log_id) = authored_log_id(&revoke_payload) {
                let _ = s2.output(FeedCardOut::Revoke { log_id });
            }
        });

        // Share button → emit ShareToCircle with the interp_key/body.
        // `DocEdited` has no body of its own — sent empty, and
        // `AppMsg::OpenShareToCirclePicker` loads the doc's current text
        // instead (see `can_share`'s doc comment).
        let share_payload = self.item.payload.clone();
        let s3 = sender.clone();
        share_btn.connect_clicked(move |_| {
            match &share_payload {
                FeedPayload::InterpAuthored { interp_key, body, .. } => {
                    let _ = s3.output(FeedCardOut::ShareToCircle {
                        interp_key: interp_key.clone(), body: body.clone(),
                    });
                }
                FeedPayload::DocEdited { interp_key, .. } => {
                    let _ = s3.output(FeedCardOut::ShareToCircle {
                        interp_key: interp_key.clone(), body: String::new(),
                    });
                }
                _ => {}
            }
        });

        let _ = root;
        FeedCardWidgets { glyph, title, subtitle, footer, sub_meta, unread, revoke_btn, share_btn }
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        match msg {
            FeedCardMsg::SetRead(r) => self.read = r,
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: FactorySender<Self>) {
        widgets.unread.set_visible(false);
        widgets.revoke_btn.set_visible(is_own_authored(&self.item.payload, &self.me));
        widgets.share_btn.set_visible(can_share(&self.item.payload, &self.me));
        render_into(
            &self.item,
            &widgets.glyph, &widgets.title, &widgets.subtitle,
            &widgets.footer, &widgets.sub_meta,
        );
    }
}

// ── card text rendering ───────────────────────────────────────────────────────

fn render_into(
    item:     &FeedItem,
    glyph:    &gtk::Label,
    title:    &gtk::Label,
    subtitle: &gtk::Label,
    footer:   &gtk::Label,
    sub_meta: &gtk::Label,
) {
    let card = card_text(item);
    glyph.set_text(card.glyph);
    title.set_text(&card.title);
    subtitle.set_text(&card.subtitle);
    subtitle.set_visible(!card.subtitle.is_empty());
    footer.set_text(&card.footer);
    footer.set_visible(!card.footer.is_empty());
    sub_meta.set_text(&card.sub_meta);
    sub_meta.set_visible(!card.sub_meta.is_empty());
}

struct CardText {
    glyph:    &'static str,
    title:    String,
    subtitle: String,
    /// Right column top row — typically the date range or "Xm ago".
    footer:   String,
    /// Right column bottom row — orb degrees / pubkey-tag / metadata.
    sub_meta: String,
}

fn card_text(item: &FeedItem) -> CardText {
    use zodia_core::humanize_key;
    match &item.payload {
        FeedPayload::InterpAuthored { author, interp_key, body } => CardText {
            glyph:    "✎",
            title:    humanize_key(interp_key),
            subtitle: truncate(body, 240),
            footer:   relative_ts(item.timestamp),
            sub_meta: format!("by {}", pubkey_tag(author.as_bytes())),
        },
        FeedPayload::AffirmAdded { voter, .. } => CardText {
            glyph:    "♡",
            title:    humanize_key(item.interp_key().unwrap_or_default()),
            subtitle: String::new(),
            footer:   relative_ts(item.timestamp),
            sub_meta: format!("from {}", pubkey_tag(voter.as_bytes())),
        },
        FeedPayload::ResponseAdded { author, interp_key, body, .. } => CardText {
            glyph:    "↳",
            title:    format!("response to {}", humanize_key(interp_key)),
            subtitle: truncate(body, 240),
            footer:   relative_ts(item.timestamp),
            sub_meta: format!("by {}", pubkey_tag(author.as_bytes())),
        },
        FeedPayload::TransitActive { transit_key, start_jdn, end_jdn, orb_deg, baseline } => CardText {
            glyph:    "◐",
            title:    humanize_key(transit_key),
            subtitle: baseline_or_invite(baseline),
            footer:   window_str(*start_jdn, *end_jdn),
            sub_meta: format!("orb {orb_deg:.1}°"),
        },
        FeedPayload::SkyAspectActive { aspect_key, start_jdn, end_jdn, orb_deg, baseline } => CardText {
            glyph:    "✦",
            title:    humanize_key(aspect_key),
            subtitle: baseline_or_invite(baseline),
            footer:   window_str(*start_jdn, *end_jdn),
            sub_meta: format!("orb {orb_deg:.1}°"),
        },
        FeedPayload::HouseTransitActive { transit_key, start_jdn, end_jdn, baseline } => CardText {
            glyph:    "⌂",
            title:    humanize_key(transit_key),
            subtitle: baseline_or_invite(baseline),
            footer:   window_str(*start_jdn, *end_jdn),
            sub_meta: format!("entered {}", zodia_core::jdn_to_display_date(*start_jdn)),
        },

        FeedPayload::DocEdited { editor, interp_key } => CardText {
            glyph:    "✏",
            title:    humanize_key(interp_key),
            subtitle: "community reading edited".into(),
            footer:   relative_ts(item.timestamp),
            sub_meta: format!("by {}", pubkey_tag(editor.as_bytes())),
        },
        FeedPayload::BlockYouAuthoredWasEdited { editor, interp_key, .. } => CardText {
            glyph:    "⚠",
            title:    humanize_key(interp_key),
            subtitle: "your edit was modified — tap to review".into(),
            footer:   relative_ts(item.timestamp),
            sub_meta: format!("by {}", pubkey_tag(editor.as_bytes())),
        },
        FeedPayload::DocRolledBack { revoker, interp_key } => CardText {
            glyph:    "↶",
            title:    humanize_key(interp_key),
            subtitle: "edit vetoed and rolled back".into(),
            footer:   relative_ts(item.timestamp),
            sub_meta: format!("by {}", pubkey_tag(revoker.as_bytes())),
        },
        FeedPayload::DocAffirmed { voter, interp_key } => CardText {
            glyph:    "♡",
            title:    humanize_key(interp_key),
            subtitle: "revision affirmed".into(),
            footer:   relative_ts(item.timestamp),
            sub_meta: format!("by {}", pubkey_tag(voter.as_bytes())),
        },
        FeedPayload::EditorPresenceChanged { peer, interp_key, joined } => CardText {
            glyph:    if *joined { "○" } else { "●" },
            title:    humanize_key(interp_key),
            subtitle: if *joined {
                "stargazer entered the editor — join them".into()
            } else {
                "stargazer left the editor".into()
            },
            footer:   relative_ts(item.timestamp),
            sub_meta: pubkey_tag(peer.as_bytes()),
        },
    }
}

fn window_str(start_jdn: f64, end_jdn: f64) -> String {
    use zodia_core::jdn_to_display_date;
    let s = jdn_to_display_date(start_jdn);
    let e = jdn_to_display_date(end_jdn);
    if s == e { s } else { format!("{s} – {e}") }
}

/// Render the baseline interpretation in full when present; otherwise
/// surface a call-to-action so users with a missing reading are invited
/// to write the first one.
fn baseline_or_invite(baseline: &Option<String>) -> String {
    match baseline {
        Some(text) if !text.is_empty() => text.clone(),
        _ => "No reading yet — tap to write the first one".to_string(),
    }
}

fn pubkey_tag(bytes: &[u8; 32]) -> String {
    format!("···{}", hex::encode_upper(&bytes[..4]))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn relative_ts(unix_secs: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(unix_secs);
    let delta = now.saturating_sub(unix_secs);
    if delta < 60         { return "now".to_string();        }
    if delta < 3600       { return format!("{}m", delta / 60);   }
    if delta < 86_400     { return format!("{}h", delta / 3600); }
    if delta < 604_800    { return format!("{}d", delta / 86_400); }
    format!("{}w", delta / 604_800)
}

