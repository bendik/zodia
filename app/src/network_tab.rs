//! "Network" sidebar tab — discoverable peers + recent community contributions.
//!
//! Self-contained `SimpleComponent` that owns its toolbar, header, scrollable
//! body, and the dynamic widgets inside.  Parent ships a `Refresh` snapshot
//! whenever `network_changed_token` advances; the component clears and rebuilds
//! its content list internally.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::prelude::*;
use relm4::Component;
use zodia_net::PeerId;

// ── public entry point ────────────────────────────────────────────────────────

/// Snapshot of one discoverable peer — enough for the row visual and click
/// action without coupling to `Stargazer`/`StargazerState`.
#[derive(Debug, Clone)]
pub struct NetPeer {
    pub peer_id:        PeerId,
    pub solar_month:    u8,
    pub aspects:        Vec<String>,
    pub geohash_prefix: String,
}

/// Snapshot of one recent community interpretation entry.
#[derive(Debug, Clone)]
pub struct NetInterp {
    pub interp_key: String,
    pub body:       String,
}

/// One row in the "Sync activity" group — derived from `AppModel::sync_peer_status`.
#[derive(Debug, Clone)]
pub struct NetSyncStatus {
    /// Short 4-byte hex tag of the remote's p2panda VerifyingKey.
    pub pubkey_tag: String,
    /// Human-readable status line: "Syncing…", "Caught up · 14 ops", "Failed".
    pub label:      String,
}

/// Spawn the Network tab, return its `ToolbarView` root + an input sender.
/// `split_view` is captured for the sidebar toggle button.
pub fn launch<F, M>(
    split_view: adw::NavigationSplitView,
    parent_input: &relm4::Sender<M>,
    map: F,
) -> (adw::ToolbarView, relm4::Sender<NetworkTabMsg>)
where
    F: Fn(NetworkTabOut) -> M + Send + 'static,
    M: Send + 'static,
{
    use relm4::ComponentController;
    let mut ctl = <NetworkTab as Component>::builder()
        .launch(split_view)
        .forward(parent_input, map);
    let widget = ctl.widget().clone();
    let sender = ctl.sender().clone();
    ctl.detach_runtime();
    (widget, sender)
}

// ── messages ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum NetworkTabMsg {
    SetStatus(String),
    /// This device's own display name, if one has been set — kept
    /// separately from `SetStatus`'s composed status line so the edit
    /// dialog can prefill with the raw name.
    SetOwnName(Option<String>),
    Refresh {
        peers:       Vec<NetPeer>,
        recent:      Vec<NetInterp>,
        sync_status: Vec<NetSyncStatus>,
    },
}

#[derive(Debug)]
pub enum NetworkTabOut {
    ProposeConsent(PeerId),
    SetOwnDisplayName(String),
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct NetworkTab {
    status:      String,
    /// This device's own display name, if set. `Rc<RefCell<_>>` so the
    /// name-edit dialog (built once in `init`, no `&self` access at click
    /// time) can read the live value — same pattern as `PeerRow`'s
    /// `nickname` cell.
    own_name:    Rc<RefCell<Option<String>>>,
    peers:       Vec<NetPeer>,
    recent:      Vec<NetInterp>,
    sync_status: Vec<NetSyncStatus>,
    /// Bumped on every Refresh so update_view rebuilds the dynamic content.
    /// SetStatus does not bump this — only the status label is touched.
    refresh_token:        u64,
    refresh_token_shown:  std::cell::Cell<u64>,
}

pub struct NetworkTabWidgets {
    status_label:  gtk::Label,
    name_edit_btn: gtk::Button,
    /// Vertical box that holds a header row (status label + name-edit
    /// button, always first child) plus the dynamic `peers`/`recent`
    /// groups appended by `rebuild`.
    content_box:   gtk::Box,
    sidebar_btn:   gtk::Button,
}

impl SimpleComponent for NetworkTab {
    type Init    = adw::NavigationSplitView;
    type Input   = NetworkTabMsg;
    type Output  = NetworkTabOut;
    type Root    = adw::ToolbarView;
    type Widgets = NetworkTabWidgets;

    fn init_root() -> Self::Root {
        adw::ToolbarView::new()
    }

    fn init(
        split_view: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // ── header ────────────────────────────────────────────────────────────
        let sidebar_btn = crate::app::sidebar_toggle_button(&split_view);

        relm4::view! {
            header = adw::HeaderBar {
                set_show_start_title_buttons: false,
                set_show_end_title_buttons: false,
                // We already show our own sidebar-toggle button for "go
                // back to the sidebar" on narrow windows; AdwHeaderBar's
                // automatic back button would otherwise duplicate it.
                set_show_back_button: false,

                #[wrap(Some)]
                set_title_widget = &gtk::Label {
                    set_label: "Network",
                    add_css_class: "title",
                },
            }
        }
        #[cfg(not(target_os = "macos"))]
        header.pack_start(&sidebar_btn);
        #[cfg(target_os = "macos")]
        header.pack_end(&sidebar_btn);
        root.add_top_bar(&header);

        // ── body ──────────────────────────────────────────────────────────────
        relm4::view! {
            status_label = gtk::Label {
                set_label: "Starting up…",
                add_css_class: "dimmed",
                add_css_class: "caption",
            }
        }
        relm4::view! {
            name_edit_btn = gtk::Button {
                set_icon_name: "document-edit-symbolic",
                add_css_class: "flat",
                add_css_class: "circular",
                set_tooltip_text: Some("Set your display name"),
                set_valign: gtk::Align::Center,
            }
        }
        relm4::view! {
            status_row = gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_halign: gtk::Align::Center,
                set_spacing: 4,
                set_margin_top: 8,
                append: &status_label,
                append: &name_edit_btn,
            }
        }
        let own_name = Rc::new(RefCell::new(None::<String>));
        {
            let s = sender.clone();
            let btn_ref = name_edit_btn.clone();
            let name_cell = Rc::clone(&own_name);
            name_edit_btn.connect_clicked(move |_| {
                let current = name_cell.borrow().clone().unwrap_or_default();
                let dialog = adw::AlertDialog::new(Some("Set Your Display Name"), Some(
                    "Shown to other peers instead of your hex id. Leave blank to clear.",
                ));
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("set", "Set");
                dialog.set_response_appearance("set", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("set"));
                dialog.set_close_response("cancel");
                let entry = gtk::Entry::new();
                entry.set_text(&current);
                entry.set_placeholder_text(Some("Display name…"));
                dialog.set_extra_child(Some(&entry));
                let s_inner = s.clone();
                let e = entry.clone();
                dialog.connect_response(None, move |_, id| {
                    if id == "set" {
                        let _ = s_inner.output(NetworkTabOut::SetOwnDisplayName(e.text().to_string()));
                    }
                });
                dialog.present(Some(&btn_ref));
            });
        }
        relm4::view! {
            content_box = gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 16,
                append: &status_row,
            }
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

        ComponentParts {
            model: NetworkTab {
                status:              String::new(),
                own_name:            own_name,
                peers:               Vec::new(),
                recent:              Vec::new(),
                sync_status:         Vec::new(),
                refresh_token:       0,
                refresh_token_shown: std::cell::Cell::new(u64::MAX),
            },
            widgets: NetworkTabWidgets { status_label, name_edit_btn, content_box, sidebar_btn },
        }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            NetworkTabMsg::SetStatus(s) => {
                self.status = s;
            }
            NetworkTabMsg::SetOwnName(name) => {
                *self.own_name.borrow_mut() = name;
            }
            NetworkTabMsg::Refresh { peers, recent, sync_status } => {
                self.peers       = peers;
                self.recent      = recent;
                self.sync_status = sync_status;
                self.refresh_token = self.refresh_token.wrapping_add(1);
            }
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, sender: ComponentSender<Self>) {
        let _ = &widgets.sidebar_btn;   // visibility handled by signal handler
        let _ = &widgets.name_edit_btn; // click handled by signal handler set up in init

        widgets.status_label.set_text(&self.status);

        // Skip the dynamic rebuild unless the refresh token changed.
        if self.refresh_token_shown.get() == self.refresh_token {
            return;
        }
        self.refresh_token_shown.set(self.refresh_token);

        // Drop everything except the first child (status_label).
        loop {
            match widgets.content_box.last_child() {
                Some(child) if child != widgets.content_box.first_child().unwrap() => {
                    widgets.content_box.remove(&child);
                }
                _ => break,
            }
        }

        if self.peers.is_empty() {
            let status = adw::StatusPage::new();
            status.set_icon_name(Some("network-wireless-symbolic"));
            status.set_title("No current online Zodia users found");
            status.set_description(Some(
                "Other Zodia users will appear here as they are discovered.",
            ));
            widgets.content_box.append(&status);
        } else {
            let group = adw::PreferencesGroup::new();
            let n = self.peers.len();
            group.set_title(&format!(
                "{n} user{} on the network",
                if n == 1 { "" } else { "s" }
            ));

            for dp in &self.peers {
                let glyph = crate::util::sign_glyph(dp.solar_month);
                let aspects = if dp.aspects.is_empty() {
                    "—".to_string()
                } else {
                    dp.aspects.join("  ")
                };
                let row = adw::ActionRow::new();
                row.set_title(&aspects);
                row.set_subtitle(&dp.geohash_prefix);

                let glyph_lbl = gtk::Label::new(Some(glyph));
                glyph_lbl.set_valign(gtk::Align::Center);
                row.add_prefix(&glyph_lbl);
                row.set_activatable(false);

                let add_btn = gtk::Button::new();
                add_btn.set_icon_name("list-add-symbolic");
                add_btn.add_css_class("flat");
                add_btn.set_valign(gtk::Align::Center);
                add_btn.set_tooltip_text(Some("Exchange charts"));
                let pid = dp.peer_id.clone();
                let s = sender.clone();
                add_btn.connect_clicked(move |_| {
                    let _ = s.output(NetworkTabOut::ProposeConsent(pid.clone()));
                });
                row.add_suffix(&add_btn);

                group.add(&row);
            }
            widgets.content_box.append(&group);
        }

        if !self.recent.is_empty() {
            let contrib_group = adw::PreferencesGroup::new();
            contrib_group.set_title("Recent Contributions");
            contrib_group.set_description(Some("Interpretations shared across the network"));

            for interp in &self.recent {
                let (kind, desc) = format_interp_key(&interp.interp_key);

                let row = adw::ActionRow::new();
                row.set_title(&desc);

                let preview = if interp.body.len() > 120 {
                    format!("{}…", &interp.body[..120])
                } else {
                    interp.body.clone()
                };
                row.set_subtitle(&preview);
                row.set_subtitle_lines(2);
                row.set_activatable(false);

                let kind_lbl = gtk::Label::new(Some(&kind));
                kind_lbl.add_css_class("caption");
                kind_lbl.add_css_class("dimmed");
                kind_lbl.set_valign(gtk::Align::Center);
                row.add_prefix(&kind_lbl);

                contrib_group.add(&row);
            }
            widgets.content_box.append(&contrib_group);
        }

        if !self.sync_status.is_empty() {
            let sync_group = adw::PreferencesGroup::new();
            sync_group.set_title("Sync activity");
            sync_group.set_description(Some(
                "Catch-up sessions with other Zodia users sharing the community log",
            ));

            for entry in &self.sync_status {
                let row = adw::ActionRow::new();
                row.set_title(&format!("···{}", entry.pubkey_tag));
                row.set_subtitle(&entry.label);
                row.set_activatable(false);
                sync_group.add(&row);
            }

            widgets.content_box.append(&sync_group);
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn format_interp_key(key: &str) -> (String, String) {
    let (kind, rest) = key.split_once(':').unwrap_or(("", key));
    let kind_label = match kind {
        "natal"           => "Natal",
        "synastry"        => "Synastry",
        "transit"         => "Transit",
        "house_transit"   => "House transit",
        "placement"       => "Placement",
        "placement_angle" => "Placement (angle)",
        other             => other,
    };
    let desc = rest.replace('_', " ");
    let mut chars = desc.chars();
    let desc = match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None    => desc,
    };
    (kind_label.to_string(), desc)
}
