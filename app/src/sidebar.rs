//! Left-hand navigation sidebar for the main page.
//!
//! Owns the `Zodia` header (with notification bell), the static nav list
//! (Chart / Sky / Network), the "Others" section header, and a slot for the
//! parent's peers `FactoryVecDeque` widget.
//!
//! Switches the parent's content `gtk::Stack` directly when the user picks a
//! static nav row — the stack reference is passed in at construction time so
//! the activation closure can call `set_visible_child_name` synchronously on
//! the GTK main thread, no message round-trip required.

use std::cell::Cell;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::prelude::*;
use relm4::Component;

// ── public entry point ────────────────────────────────────────────────────────

pub struct SidebarInit {
    pub peers_list:         gtk::ListBox,
    pub discussions_list:   gtk::ListBox,
    pub discussions_header: gtk::Label,
    /// Private circles this device created or was invited into — one row
    /// per circle, each opening its own dynamically-added page (mirrors how
    /// connected-peer pages work). Circles rank above `peers_list` ("direct
    /// peer contact") since the user asked for circles to take precedence,
    /// but below the static Chart/Sky/Network rows.
    pub circles_list:       gtk::ListBox,
    pub circles_header:     gtk::Label,
    pub notif_widget:       gtk::MenuButton,
    pub split_view:         adw::OverlaySplitView,
    pub content_stack:      gtk::Stack,
}

/// Spawn the sidebar, return its `ToolbarView` root + an input sender.
pub fn launch(init: SidebarInit) -> (adw::ToolbarView, relm4::Sender<SidebarMsg>) {
    use relm4::ComponentController;
    let mut ctl = <Sidebar as Component>::builder()
        .launch(init)
        .detach();
    let widget = ctl.widget().clone();
    let sender = ctl.sender().clone();
    ctl.detach_runtime();
    (widget, sender)
}

// ── messages ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SidebarMsg {
    /// Show or hide the "Others" section header (peers factory).
    SetOthersVisible(bool),
    /// Clear the static nav-list selection (used when a peer page is opened).
    UnselectNav,
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct Sidebar {
    others_visible:        bool,
    /// Bumped on each UnselectNav input; update_view triggers unselect_all
    /// when the shown counter differs from this one.
    unselect_token:        u64,
    unselect_token_shown:  Cell<u64>,
}

pub struct SidebarWidgets {
    nav_list:      gtk::ListBox,
    others_header: gtk::Label,
}

impl SimpleComponent for Sidebar {
    type Init    = SidebarInit;
    type Input   = SidebarMsg;
    type Output  = ();
    type Root    = adw::ToolbarView;
    type Widgets = SidebarWidgets;

    fn init_root() -> Self::Root {
        adw::ToolbarView::new()
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let SidebarInit {
            peers_list, discussions_list, discussions_header,
            circles_list, circles_header,
            notif_widget, split_view, content_stack,
        } = init;

        // ── Static nav list (Chart / Sky / Network) ──────────────────────────
        relm4::view! {
            nav_list = gtk::ListBox {
                add_css_class: "navigation-sidebar",
                set_selection_mode: gtk::SelectionMode::Single,
            }
        }

        let make_nav_row = |icon: &str, label_text: &str| -> gtk::ListBoxRow {
            relm4::view! {
                row = gtk::ListBoxRow {
                    #[wrap(Some)]
                    set_child = &gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 10,
                        set_margin_start: 12,
                        set_margin_end: 12,
                        set_margin_top: 10,
                        set_margin_bottom: 10,

                        gtk::Image {
                            set_icon_name: Some(icon),
                            set_pixel_size: 16,
                        },

                        gtk::Label {
                            set_label: label_text,
                            set_halign: gtk::Align::Start,
                        },
                    },
                }
            }
            row
        };

        nav_list.append(&make_nav_row("weather-clear-symbolic",    "Chart"));
        nav_list.append(&make_nav_row("night-light-symbolic",      "Sky"));
        nav_list.append(&make_nav_row("network-wireless-symbolic", "Network"));
        nav_list.select_row(nav_list.row_at_index(0).as_ref());

        {
            let cs = content_stack.clone();
            let sv = split_view.clone();
            let pl = peers_list.clone();
            nav_list.connect_row_activated(move |_, row| {
                let page = match row.index() {
                    0 => "chart",
                    1 => "sky",
                    2 => "network",
                    _ => return,
                };
                cs.set_visible_child_name(page);
                if sv.is_collapsed() { sv.set_show_sidebar(false); }
                pl.unselect_all();
            });
        }

        // ── "Others" section header ──────────────────────────────────────────
        let others_header = gtk::Label::new(Some("Others"));
        others_header.add_css_class("heading");
        others_header.add_css_class("dim-label");
        others_header.set_halign(gtk::Align::Start);
        others_header.set_margin_start(12);
        others_header.set_margin_end(12);
        others_header.set_margin_top(12);
        others_header.set_margin_bottom(2);
        others_header.set_visible(false);

        relm4::view! {
            sidebar_box = gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 0,
                append: &nav_list,
                append: &discussions_header,
                append: &discussions_list,
                append: &circles_header,
                append: &circles_list,
                append: &others_header,
                append: &peers_list,
            }
        }
        relm4::view! {
            sidebar_scroll = gtk::ScrolledWindow {
                set_hscrollbar_policy: gtk::PolicyType::Never,
                set_vexpand: true,
                #[wrap(Some)]
                set_child = &sidebar_box.clone() {},
            }
        }
        relm4::view! {
            sidebar_header = adw::HeaderBar {
                #[wrap(Some)]
                set_title_widget = &adw::WindowTitle::new("Zodia", ""),
            }
        }
        sidebar_header.pack_end(&notif_widget);

        root.add_top_bar(&sidebar_header);
        root.set_content(Some(&sidebar_scroll));

        ComponentParts {
            model: Sidebar {
                others_visible:       false,
                unselect_token:       0,
                unselect_token_shown: Cell::new(0),
            },
            widgets: SidebarWidgets { nav_list, others_header },
        }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            SidebarMsg::SetOthersVisible(v) => self.others_visible = v,
            SidebarMsg::UnselectNav         => {
                self.unselect_token = self.unselect_token.wrapping_add(1);
            }
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        widgets.others_header.set_visible(self.others_visible);
        if self.unselect_token_shown.get() != self.unselect_token {
            self.unselect_token_shown.set(self.unselect_token);
            widgets.nav_list.unselect_all();
        }
    }
}
