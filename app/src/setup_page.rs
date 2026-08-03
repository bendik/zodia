//! First-run birth-data form, as a self-contained `SimpleComponent`.
//!
//! Owns date/time spin rows, the city-search section, manual-coordinates
//! fallback, and the Begin button.  Forwards a `Submit` output to the parent
//! when the user taps Begin with valid input; an `Error` output for local
//! validation failures (e.g. no location selected).  An `SetError` input lets
//! the parent display upstream errors (e.g. coordinates out of range).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::prelude::*;
use relm4::Component;

// ── public entry point ────────────────────────────────────────────────────────

/// Spawn the setup form and detach.  Returns the ToolbarView root and a
/// sender for `SetupPageMsg::SetError`.  Outputs are forwarded to
/// `parent_input` via `map`.
pub fn launch<F, M>(
    parent_input: &relm4::Sender<M>,
    map: F,
) -> (adw::ToolbarView, relm4::Sender<SetupPageMsg>)
where
    F: Fn(SetupPageOut) -> M + Send + 'static,
    M: Send + 'static,
{
    use relm4::ComponentController;
    let mut ctl = <SetupPage as Component>::builder()
        .launch(())
        .forward(parent_input, map);
    let widget = ctl.widget().clone();
    let sender = ctl.sender().clone();
    ctl.detach_runtime();
    (widget, sender)
}

// ── messages ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SetupPageMsg {
    SetError(String),
}

#[derive(Debug)]
pub enum SetupPageOut {
    Submit {
        year:         i32,
        month:        u32,
        day:          u32,
        hour:         u32,
        minute:       u32,
        lat:          f64,
        lon:          f64,
        display_name: String,
    },
    Error(String),
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct SetupPage {
    error: String,
}

pub struct SetupPageWidgets {
    setup_status: gtk::Label,
}

// ── component ─────────────────────────────────────────────────────────────────

impl SimpleComponent for SetupPage {
    type Init    = ();
    type Input   = SetupPageMsg;
    type Output  = SetupPageOut;
    type Root    = adw::ToolbarView;
    type Widgets = SetupPageWidgets;

    fn init_root() -> Self::Root {
        adw::ToolbarView::new()
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = SetupPage { error: String::new() };

        // ── header ────────────────────────────────────────────────────────────
        relm4::view! {
            header_bar = adw::HeaderBar {
                #[wrap(Some)]
                set_title_widget = &gtk::Label {
                    set_label: "Zodia",
                    add_css_class: "title",
                },
            }
        }
        root.add_top_bar(&header_bar);

        // ── display name ──────────────────────────────────────────────────────
        relm4::view! {
            name_row = adw::EntryRow {
                set_title: "Your name",
                set_input_purpose: gtk::InputPurpose::Name,
            }
        }
        relm4::view! {
            name_group = adw::PreferencesGroup {
                set_title: "Introduce Yourself",
                set_description: Some(
                    "Shown to other Zodia users instead of a hex id. Optional — you can set \
                     or change this later from the Network tab.",
                ),
                add: &name_row,
            }
        }

        // ── date / time group ─────────────────────────────────────────────────
        relm4::view! {
            date_group = adw::PreferencesGroup {
                set_title: "Birth Date &amp; Time",
                set_description: Some(
                    "Enter your local birth time — timezone is resolved automatically from the birth location."
                ),
            }
        }
        let year_row   = spin_row(1900.0, 2100.0, 1.0, "Year",          1990.0);
        let month_row  = spin_row(   1.0,   12.0, 1.0, "Month",            6.0);
        let day_row    = spin_row(   1.0,   31.0, 1.0, "Day",             15.0);
        let hour_row   = spin_row(   0.0,   23.0, 1.0, "Hour (local)",    12.0);
        let minute_row = spin_row(   0.0,   59.0, 1.0, "Minute",           0.0);
        for r in [&year_row, &month_row, &day_row, &hour_row, &minute_row] {
            date_group.add(r);
        }

        let selected_loc: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));

        // ── city search section ───────────────────────────────────────────────
        relm4::view! {
            city_row = adw::EntryRow {
                set_title: "City",
                set_input_purpose: gtk::InputPurpose::Name,
                set_input_hints: gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::NO_EMOJI,
            }
        }
        relm4::view! {
            coord_row = adw::ActionRow {
                set_title: "Coordinates",
                set_subtitle: "—",
                set_selectable: false,
                set_sensitive: false,
            }
        }
        relm4::view! {
            city_group = adw::PreferencesGroup {
                set_title: "Birth Location",
                add: &city_row,
                add: &coord_row,
            }
        }
        relm4::view! {
            to_manual_btn = gtk::Button {
                set_label: "Enter coordinates manually",
                add_css_class: "flat",
            }
        }
        relm4::view! {
            city_section = gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 12,
                append: &city_group,
                append: &to_manual_btn,
            }
        }

        wire_city_search(&city_row, &coord_row, &city_group, &selected_loc);

        // ── manual lat/lon section ────────────────────────────────────────────
        let lat_row = adw::SpinRow::with_range(-90.0, 90.0, 0.0001);
        lat_row.set_title("Latitude");
        lat_row.set_digits(4);
        let lon_row = adw::SpinRow::with_range(-180.0, 180.0, 0.0001);
        lon_row.set_title("Longitude");
        lon_row.set_digits(4);
        relm4::view! {
            manual_group = adw::PreferencesGroup {
                set_title: "Birth Location",
                add: &lat_row,
                add: &lon_row,
            }
        }
        relm4::view! {
            manual_section = gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 12,
                append: &manual_group,
            }
        }

        // Live-track manual lat/lon into selected_loc when set.
        {
            let loc = Rc::clone(&selected_loc);
            lat_row.connect_value_notify(move |row| {
                if let Some(ref mut v) = *loc.borrow_mut() { v.0 = row.value(); }
            });
        }
        {
            let loc = Rc::clone(&selected_loc);
            lon_row.connect_value_notify(move |row| {
                if let Some(ref mut v) = *loc.borrow_mut() { v.1 = row.value(); }
            });
        }

        // ── visibility + section toggles ──────────────────────────────────────
        if zodia_core::has_cities() {
            manual_section.set_visible(false);
            {
                let cs   = city_section.clone();
                let ms   = manual_section.clone();
                let loc  = Rc::clone(&selected_loc);
                let latr = lat_row.clone();
                let lonr = lon_row.clone();
                to_manual_btn.connect_clicked(move |_| {
                    cs.set_visible(false);
                    ms.set_visible(true);
                    *loc.borrow_mut() = Some((latr.value(), lonr.value()));
                });
            }
            relm4::view! {
                back_btn = gtk::Button {
                    set_label: "Search by city name",
                    add_css_class: "flat",
                }
            }
            manual_section.append(&back_btn);
            {
                let cs  = city_section.clone();
                let ms  = manual_section.clone();
                let loc = Rc::clone(&selected_loc);
                back_btn.connect_clicked(move |_| {
                    ms.set_visible(false);
                    cs.set_visible(true);
                    *loc.borrow_mut() = None;
                });
            }
        } else {
            // No bundled city data: skip straight to manual, no toggle back.
            city_section.set_visible(false);
            *selected_loc.borrow_mut() = Some((0.0, 0.0));
        }

        // ── status + begin button ─────────────────────────────────────────────
        relm4::view! {
            setup_status = gtk::Label {
                add_css_class: "error",
            }
        }
        relm4::view! {
            btn = gtk::Button {
                set_label: "Begin  →",
                add_css_class: "suggested-action",
                add_css_class: "pill",
                set_halign: gtk::Align::Center,
            }
        }

        {
            let s     = sender.output_sender().clone();
            let yr    = year_row.clone();
            let mr    = month_row.clone();
            let dr    = day_row.clone();
            let hr    = hour_row.clone();
            let minr  = minute_row.clone();
            let loc   = Rc::clone(&selected_loc);
            let namer = name_row.clone();
            btn.connect_clicked(move |_| {
                let (lat, lon) = match *loc.borrow() {
                    Some(v) => v,
                    None => {
                        let _ = s.send(SetupPageOut::Error("Select a birth location".into()));
                        return;
                    }
                };
                let _ = s.send(SetupPageOut::Submit {
                    year:   yr.value() as i32,
                    month:  mr.value() as u32,
                    day:    dr.value() as u32,
                    hour:   hr.value() as u32,
                    minute: minr.value() as u32,
                    lat, lon,
                    display_name: namer.text().to_string(),
                });
            });
        }

        // ── compose content tree ──────────────────────────────────────────────
        relm4::view! {
            content = gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 24,
                set_valign: gtk::Align::Center,
                set_vexpand: true,

                gtk::Label {
                    set_label: "Welcome to Zodia",
                    add_css_class: "title-1",
                },

                gtk::Label {
                    set_label: "Enter your birth details to find your astrological neighbourhood.",
                    add_css_class: "dim-label",
                    set_wrap: true,
                    set_max_width_chars: 50,
                },

                append: &name_group,
                append: &date_group,
                append: &city_section,
                append: &manual_section,
                append: &setup_status,
                append: &btn,
            }
        }
        relm4::view! {
            scroll = gtk::ScrolledWindow {
                set_hscrollbar_policy: gtk::PolicyType::Never,
                set_vexpand: true,
                #[wrap(Some)]
                set_child = &adw::Clamp {
                    set_maximum_size: 480,
                    set_margin_top: 24,
                    set_margin_bottom: 24,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    #[wrap(Some)]
                    set_child = &content.clone() {},
                },
            }
        }
        root.set_content(Some(&scroll));

        let widgets = SetupPageWidgets { setup_status };
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            SetupPageMsg::SetError(e) => self.error = e,
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        widgets.setup_status.set_text(&self.error);
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn spin_row(min: f64, max: f64, step: f64, title: &str, init: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    row.set_value(init);
    row
}

/// Wire the city EntryRow's `connect_changed` signal to maintain inline
/// search-result rows in `city_group`.  Picking a result updates `selected_loc`.
fn wire_city_search(
    city_row:     &adw::EntryRow,
    coord_row:    &adw::ActionRow,
    city_group:   &adw::PreferencesGroup,
    selected_loc: &Rc<RefCell<Option<(f64, f64)>>>,
) {
    let result_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let selecting:   Rc<Cell<bool>>                    = Rc::new(Cell::new(false));
    let loc     = Rc::clone(selected_loc);
    let coord_r = coord_row.clone();
    let group   = city_group.clone();
    let rows    = result_rows.clone();
    let sel     = selecting.clone();
    let entry   = city_row.clone();

    city_row.connect_changed(move |e| {
        if sel.get() { return; }

        {
            let to_remove: Vec<_> = rows.borrow_mut().drain(..).collect();
            for r in &to_remove { group.remove(r); }
        }
        group.remove(&coord_r);

        *loc.borrow_mut() = None;
        coord_r.set_subtitle("—");
        coord_r.set_sensitive(false);

        let text = e.text();
        if !text.is_empty() {
            let mut rs = rows.borrow_mut();
            for hit in zodia_core::search_cities(text.as_str(), 8) {
                let label = format!("{}, {}", hit.name, hit.country);
                let lat   = hit.lat as f64;
                let lon   = hit.lon as f64;
                let row   = adw::ActionRow::new();
                row.set_title(&label);
                row.set_activatable(true);

                let loc2     = Rc::clone(&loc);
                let coord_r2 = coord_r.clone();
                let group2   = group.clone();
                let rows2    = rows.clone();
                let sel2     = sel.clone();
                let entry2   = entry.clone();

                row.connect_activated(move |_| {
                    let to_remove: Vec<_> = rows2.borrow_mut().drain(..).collect();
                    for r in &to_remove { group2.remove(r); }

                    sel2.set(true);
                    entry2.set_text(&label);
                    sel2.set(false);

                    *loc2.borrow_mut() = Some((lat, lon));
                    coord_r2.set_subtitle(&format!("{:.4}°  {:.4}°", lat, lon));
                    coord_r2.set_sensitive(true);
                });

                rs.push(row.clone());
                group.add(&row);
            }
        }

        group.add(&coord_r);
    });
}
