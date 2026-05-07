//! "Network" sidebar tab — discoverable peers + recent community contributions.
//!
//! Self-contained `SimpleComponent` that owns its toolbar, header, scrollable
//! body, and the dynamic widgets inside.  Parent ships a `Refresh` snapshot
//! whenever `network_changed_token` advances; the component clears and rebuilds
//! its content list internally.

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

/// Spawn the Network tab, return its `ToolbarView` root + an input sender.
/// `split_view` is captured for the sidebar toggle button.
pub fn launch<F, M>(
    split_view: adw::OverlaySplitView,
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
    Refresh {
        peers:  Vec<NetPeer>,
        recent: Vec<NetInterp>,
    },
}

#[derive(Debug)]
pub enum NetworkTabOut {
    ProposeConsent(PeerId),
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct NetworkTab {
    status: String,
    peers:  Vec<NetPeer>,
    recent: Vec<NetInterp>,
    /// Bumped on every Refresh so update_view rebuilds the dynamic content.
    /// SetStatus does not bump this — only the status label is touched.
    refresh_token:        u64,
    refresh_token_shown:  std::cell::Cell<u64>,
}

pub struct NetworkTabWidgets {
    status_label:  gtk::Label,
    /// Vertical box that holds `status_label` (always first child) plus the
    /// dynamic `peers`/`recent` groups appended by `rebuild`.
    content_box:   gtk::Box,
    sidebar_btn:   gtk::Button,
}

impl SimpleComponent for NetworkTab {
    type Init    = adw::OverlaySplitView;
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
        relm4::view! {
            sidebar_btn = gtk::Button {
                set_icon_name: "open-menu-symbolic",
                set_tooltip_text: Some("Show sidebar"),
                set_visible: split_view.is_collapsed(),
            }
        }
        {
            let sv = split_view.clone();
            sidebar_btn.connect_clicked(move |_| sv.set_show_sidebar(true));
        }
        {
            let btn = sidebar_btn.clone();
            split_view.connect_notify_local(Some("collapsed"), move |sv, _| {
                btn.set_visible(sv.is_collapsed());
            });
        }

        relm4::view! {
            header = adw::HeaderBar {
                set_show_start_title_buttons: false,
                set_show_end_title_buttons: false,

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
                add_css_class: "dim-label",
                add_css_class: "caption",
                set_halign: gtk::Align::Center,
                set_margin_top: 8,
            }
        }
        relm4::view! {
            content_box = gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 16,
                append: &status_label,
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

        let _ = sender; // outputs forwarded by row clicks below

        ComponentParts {
            model: NetworkTab {
                status:              String::new(),
                peers:               Vec::new(),
                recent:               Vec::new(),
                refresh_token:        0,
                refresh_token_shown:  std::cell::Cell::new(u64::MAX),
            },
            widgets: NetworkTabWidgets { status_label, content_box, sidebar_btn },
        }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            NetworkTabMsg::SetStatus(s) => {
                self.status = s;
            }
            NetworkTabMsg::Refresh { peers, recent } => {
                self.peers  = peers;
                self.recent = recent;
                self.refresh_token = self.refresh_token.wrapping_add(1);
            }
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, sender: ComponentSender<Self>) {
        let _ = &widgets.sidebar_btn; // visibility handled by signal handler

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
                kind_lbl.add_css_class("dim-label");
                kind_lbl.set_valign(gtk::Align::Center);
                row.add_prefix(&kind_lbl);

                contrib_group.add(&row);
            }
            widgets.content_box.append(&contrib_group);
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
        "placement_sign"  => "Placement (sign)",
        "placement_house" => "Placement (house)",
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
