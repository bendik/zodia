//! Root application component.
//!
//! `AppModel` is an `AsyncComponent` that drives the full lifecycle:
//!   1. First-run setup  — collect birth date + location, compute chart
//!   2. Main view        — Chart / Sky / Peers tabs in an `adw::ToolbarView`
//!   3. Connected peer   — pushed onto the Peers tab's own `NavigationView`;
//!                         shows synastry + call interface
//!   4. Network events   — `CommandOutput = ZodiaNetEvent` keeps all three
//!                         tabs reactive without blocking the GTK thread

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::prelude::*;
use tokio::sync::mpsc::Receiver;
use tracing::{error, info, warn};
use zodia_av::AudioSession;
use zodia_config::LocalConfig;
use zodia_core::{birth_from_coords, compute_synastry, current_jdn, gregorian_to_jdn,
                 Chart, InterpKey};
use zodia_crypto::IdentityKeypair;
use zodia_net::{ChannelMsg, DirectChannel, InterpEntry, NetworkConfig, PeerId, Tier1Blob,
                ZodiaNetEvent, ZodiaNetwork};
use zodia_store::{StoreError, ZodiaStore};

use crate::aspect_list;
use crate::aspect_view::AspectView;
use crate::peer_list::DiscoveredPeer;
use crate::peer_page::{self, append_chat_row};
use crate::util::{approximate_aspects, sign_glyph};

// ── init ──────────────────────────────────────────────────────────────────────

pub struct AppInit {
    pub config: LocalConfig,
    pub store: ZodiaStore,
}

// ── call state ────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub enum CallState {
    #[default]
    Idle,
    Calling  { peer_id: PeerId },
    Ringing  { peer_id: PeerId, session_id: [u8; 32] },
    Active   { peer_id: PeerId },
}

impl CallState {
    fn active_peer(&self) -> Option<PeerId> {
        match self {
            CallState::Calling { peer_id }
            | CallState::Ringing { peer_id, .. }
            | CallState::Active  { peer_id } => Some(peer_id.clone()),
            CallState::Idle => None,
        }
    }
}

// ── messages ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
pub enum AppMsg {
    ConfirmBirth {
        year: i32, month: u32, day: u32,
        hour: u32, minute: u32,
        lat: f64, lon: f64,
    },
    SetupError(String),
    /// User tapped a peer row — connect (if needed) then open their page.
    OpenPeer(PeerId),
    CallPeer(PeerId),
    AcceptCall,
    RejectCall,
    HangUp,
    /// User sent a chat message to a connected peer.
    SendChat { peer_id: PeerId, text: String },
    /// User set or updated a nickname for a connected peer.
    SetNickname { peer_id: PeerId, name: String },
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct AppModel {
    on_setup_page: bool,
    chart: Option<Chart>,

    store: Rc<RefCell<ZodiaStore>>,

    network: Option<ZodiaNetwork>,
    node_id_text: String,

    /// Peers seen on the gossip swarm (Tier-0), ordered by discovery time.
    discovered_peers: Vec<DiscoveredPeer>,
    /// Peers whose Tier-1 exchange has completed.
    connected_peers: HashMap<PeerId, Tier1Blob>,
    /// Active QUIC channels — presence means currently online.
    connected_channels: HashMap<PeerId, DirectChannel>,

    /// Incremented whenever the peer list content changes so `update_view`
    /// knows when to rebuild the GTK rows.
    peer_list_generation: u64,

    /// Peers the user has explicitly tapped; pages pushed once Tier-1 completes.
    /// Uses `RefCell` for interior mutability inside `update_view (&self)`.
    pending_push_queue: RefCell<Vec<PeerId>>,

    config: LocalConfig,
    setup_error: String,

    identity: Rc<IdentityKeypair>,

    call_state: CallState,
    active_audio: Option<AudioSession>,

    /// Chat history per peer: `(from_us, text)`.
    chat_logs: HashMap<PeerId, Vec<(bool, String)>>,

    /// User-assigned nicknames, keyed by 4-byte upper-hex peer tag.
    peer_nicknames: HashMap<String, String>,
    /// Unread message counts per peer (cleared when their page is opened).
    unread_messages: HashMap<String, usize>,
}

// ── widgets ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct AppWidgets {
    outer_stack: gtk::Stack,
    setup_status: gtk::Label,

    chart_container: gtk::Box,
    sky_container: gtk::Box,

    /// The `NavigationView` that lives *inside* the Peers tab.
    /// Peer detail pages are pushed onto this — the tab bar always stays visible.
    peers_nav: adw::NavigationView,
    /// The box whose children are rebuilt whenever `peer_list_generation` changes.
    peers_content: gtk::Box,
    /// Generation of the peer list we last rendered.
    peer_list_shown_gen: u64,

    /// Message list widget per peer (keyed by 4-byte hex tag).
    peer_msg_lists: HashMap<String, gtk::ListBox>,
    /// How many messages from `chat_logs` have already been appended to each list.
    peer_chat_shown: HashMap<String, usize>,

    peers_page: adw::ViewStackPage,

    /// Network status button (header bar, end side) — click for node info popover.
    net_status_btn: gtk::MenuButton,
    /// Label inside the network status popover — updated with peer/sync counts.
    net_popover_label: gtk::Label,
    /// Bell button — only visible when there are unread messages.
    notif_btn: gtk::MenuButton,
    /// Label inside the notification popover — lists unread counts.
    notif_label: gtk::Label,

    call_bar: gtk::Box,
    call_status: gtk::Label,
    accept_btn: gtk::Button,
    hangup_btn: gtk::Button,
}

// ── async component ───────────────────────────────────────────────────────────

impl AsyncComponent for AppModel {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = ZodiaNetEvent;
    type Root = adw::ApplicationWindow;
    type Widgets = AppWidgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::new(&relm4::main_application())
    }

    async fn init(
        init: Self::Init,
        root: Self::Root,
        sender: AsyncComponentSender<Self>,
    ) -> AsyncComponentParts<Self> {
        let identity = Rc::new(IdentityKeypair::from_seed(init.config.identity.seed()));
        let has_birth = init.config.birth.is_some();
        let store = Rc::new(RefCell::new(init.store));

        let peer_nicknames = load_nicknames(init.config.data_dir());

        let mut model = AppModel {
            on_setup_page: !has_birth,
            chart: None,
            store,
            network: None,
            node_id_text: String::new(),
            discovered_peers: Vec::new(),
            connected_peers: HashMap::new(),
            connected_channels: HashMap::new(),
            peer_list_generation: 0,
            pending_push_queue: RefCell::new(Vec::new()),
            config: init.config,
            setup_error: String::new(),
            identity,
            call_state: CallState::Idle,
            active_audio: None,
            chat_logs: HashMap::new(),
            peer_nicknames,
            unread_messages: HashMap::new(),
        };

        if let Some(birth) = model.config.birth.clone() {
            if let Ok(chart) = Chart::compute(birth.clone()) {
                model.chart = Some(chart);
            }
        }

        let widgets = build_widgets(&root, &model, &sender);

        if let Some(birth) = model.config.birth.clone() {
            if let Some((net, rx)) = try_spawn_network(&model.config, &birth).await {
                model.node_id_text = {
                    let nid = net.node_id();
                    hex::encode_upper(&nid.0[..4])
                };
                info!("network up, node ···{}", model.node_id_text);
                let _ = net.publish_announce().await;
                model.network = Some(net);
                start_network_command(&sender, rx);
            }
        }

        AsyncComponentParts { model, widgets }
    }

    async fn update(
        &mut self,
        msg: AppMsg,
        sender: AsyncComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            AppMsg::ConfirmBirth { year, month, day, hour, minute, lat, lon } => {
                if lat < -90.0 || lat > 90.0 || lon < -180.0 || lon > 180.0 {
                    sender.input(AppMsg::SetupError(
                        "Latitude must be −90…90, longitude −180…180".to_string(),
                    ));
                    return;
                }
                let jdn = gregorian_to_jdn(year, month, day,
                    hour as f64 + minute as f64 / 60.0);
                let birth = birth_from_coords(jdn, lat, lon, 9);

                if let Err(e) = self.config.save_birth(birth.clone()) {
                    sender.input(AppMsg::SetupError(e.to_string()));
                    return;
                }
                match Chart::compute(birth.clone()) {
                    Ok(chart) => { self.chart = Some(chart); }
                    Err(e) => {
                        sender.input(AppMsg::SetupError(e.to_string()));
                        return;
                    }
                }
                if let Some((net, rx)) = try_spawn_network(&self.config, &birth).await {
                    self.node_id_text = {
                        let nid = net.node_id();
                        hex::encode_upper(&nid.0[..4])
                    };
                    let _ = net.publish_announce().await;
                    self.network = Some(net);
                    start_network_command(&sender, rx);
                }
                self.setup_error.clear();
                self.on_setup_page = false;
            }

            AppMsg::SetupError(msg) => {
                self.setup_error = msg;
            }

            AppMsg::SetNickname { peer_id, name } => {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                if name.trim().is_empty() {
                    self.peer_nicknames.remove(&tag);
                } else {
                    self.peer_nicknames.insert(tag, name.trim().to_string());
                }
                save_nicknames(self.config.data_dir(), &self.peer_nicknames);
                self.peer_list_generation += 1;
            }

            AppMsg::OpenPeer(peer_id) => {
                // Clear unread count when opening a peer's page.
                let tag = hex::encode_upper(&peer_id.0[..4]);
                self.unread_messages.remove(&tag);
                // Queue a navigation push — fulfilled in update_view once connected.
                self.pending_push_queue.borrow_mut().push(peer_id.clone());

                // Start connection if we don't already have one.
                if self.connected_peers.contains_key(&peer_id) {
                    return; // already connected, update_view will push the page
                }
                if let Some(net) = &self.network {
                    match net.connect_peer(&peer_id).await {
                        Ok(channel) => {
                            let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                            info!(peer = %peer_hex, "tier-1 channel opened");
                            if let Some(our_blob) = make_tier1_blob(&self.config) {
                                match channel.exchange_tier1(&our_blob).await {
                                    Ok(their_blob) => {
                                        info!(peer = %peer_hex, "tier-1 exchange complete");
                                        do_interp_sync(
                                            &channel, &their_blob,
                                            self.chart.as_ref(), &self.store,
                                            &self.identity, &peer_hex,
                                        ).await;
                                        self.connected_peers.insert(peer_id.clone(), their_blob);
                                        self.peer_list_generation += 1;
                                    }
                                    Err(e) => warn!("tier-1 exchange: {e}"),
                                }
                            }
                            net.accept_channel(peer_id.clone(), channel.clone());
                            self.connected_channels.insert(peer_id, channel);
                        }
                        Err(e) => error!("connect_peer: {e}"),
                    }
                }
            }

            AppMsg::CallPeer(peer_id) => {
                if let Some(channel) = self.connected_channels.get(&peer_id) {
                    let session_id = new_session_id(&peer_id);
                    match AudioSession::start(channel).await {
                        Ok(session) => {
                            let _ = channel.send_msg(&ChannelMsg::CallOffer { session_id }).await;
                            self.active_audio = Some(session);
                            self.call_state = CallState::Calling { peer_id };
                            info!("call offer sent");
                        }
                        Err(e) => error!("start audio session: {e}"),
                    }
                }
            }

            AppMsg::AcceptCall => {
                if let CallState::Ringing { peer_id, session_id } = &self.call_state {
                    let peer_id = peer_id.clone();
                    let session_id = *session_id;
                    if let Some(channel) = self.connected_channels.get(&peer_id) {
                        match AudioSession::start(channel).await {
                            Ok(session) => {
                                let _ = channel.send_msg(&ChannelMsg::CallAccept { session_id }).await;
                                self.active_audio = Some(session);
                                self.call_state = CallState::Active { peer_id };
                                info!("call accepted");
                            }
                            Err(e) => error!("start audio session: {e}"),
                        }
                    }
                }
            }

            AppMsg::RejectCall => {
                if let CallState::Ringing { peer_id, session_id } = &self.call_state {
                    let session_id = *session_id;
                    if let Some(channel) = self.connected_channels.get(peer_id) {
                        let _ = channel.send_msg(&ChannelMsg::CallReject { session_id }).await;
                    }
                }
                self.call_state = CallState::Idle;
            }

            AppMsg::HangUp => {
                if let Some(peer_id) = self.call_state.active_peer() {
                    if let Some(channel) = self.connected_channels.get(&peer_id) {
                        let _ = channel.send_msg(&ChannelMsg::CallHangup {
                            session_id: [0u8; 32],
                        }).await;
                    }
                }
                self.active_audio = None;
                self.call_state = CallState::Idle;
                info!("hung up");
            }

            AppMsg::SendChat { peer_id, text } => {
                if let Some(channel) = self.connected_channels.get(&peer_id) {
                    if channel.send_msg(&ChannelMsg::ChatMsg { text: text.clone() }).await.is_ok() {
                        self.chat_logs.entry(peer_id).or_default().push((true, text));
                    }
                }
            }
        }
    }

    async fn update_cmd(
        &mut self,
        event: ZodiaNetEvent,
        _sender: AsyncComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match event {
            ZodiaNetEvent::PeerDiscovered { peer_id, blob } => {
                let approx = self.chart.as_ref()
                    .map(|c| approximate_aspects(blob.solar_month, &c.positions))
                    .unwrap_or_default();
                self.discovered_peers.push(DiscoveredPeer::from_blob(peer_id, &blob, approx));
                self.peer_list_generation += 1;
            }
            ZodiaNetEvent::PeerLeft { peer_id } => {
                self.discovered_peers.retain(|p| p.peer_id != peer_id);
                self.peer_list_generation += 1;
            }
            ZodiaNetEvent::IncomingChannel { peer_id, channel } => {
                if let Some(net) = &self.network {
                    let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                    if let Some(our_blob) = make_tier1_blob(&self.config) {
                        match channel.exchange_tier1(&our_blob).await {
                            Ok(their_blob) => {
                                info!(peer = %peer_hex, "tier-1 exchange complete (incoming)");
                                do_interp_sync(
                                    &channel, &their_blob,
                                    self.chart.as_ref(), &self.store,
                                    &self.identity, &peer_hex,
                                ).await;
                                self.connected_peers.insert(peer_id.clone(), their_blob);
                                self.peer_list_generation += 1;
                            }
                            Err(e) => warn!("tier-1 exchange (incoming): {e}"),
                        }
                    }
                    net.accept_channel(peer_id.clone(), channel.clone());
                    self.connected_channels.insert(peer_id, channel);
                }
            }
            ZodiaNetEvent::CallOffer { from, session_id } => {
                self.call_state = CallState::Ringing { peer_id: from, session_id };
            }
            ZodiaNetEvent::CallAccepted { from, .. } => {
                self.call_state = CallState::Active { peer_id: from };
            }
            ZodiaNetEvent::CallRejected { .. } => {
                self.active_audio = None;
                self.call_state = CallState::Idle;
            }
            ZodiaNetEvent::CallHungUp { .. } => {
                self.active_audio = None;
                self.call_state = CallState::Idle;
            }
            ZodiaNetEvent::ChatReceived { from, text } => {
                let tag = hex::encode_upper(&from.0[..4]);
                *self.unread_messages.entry(tag).or_insert(0) += 1;
                self.chat_logs.entry(from).or_default().push((false, text));
            }
            _ => {}
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, sender: AsyncComponentSender<Self>) {
        // ── setup / main stack ────────────────────────────────────────────────

        widgets.outer_stack.set_visible_child_name(
            if self.on_setup_page { "setup" } else { "main" }
        );
        widgets.setup_status.set_text(&self.setup_error);

        // ── lazily populate aspect views ──────────────────────────────────────

        if !self.on_setup_page && widgets.chart_container.first_child().is_none() {
            if let Some(chart) = &self.chart {
                let nav = AspectView::natal(
                    aspect_list::natal_items(&chart.natal_aspects()),
                    chart,
                    Rc::clone(&self.store),
                    Rc::clone(&self.identity),
                );
                nav.widget().set_vexpand(true);
                widgets.chart_container.append(nav.widget());

                if let Ok(ts) = chart.transits_at(current_jdn()) {
                    let tav = AspectView::transits(
                        aspect_list::transit_items(&ts.transit_aspects, &ts.house_transits),
                        Rc::clone(&self.store),
                        Rc::clone(&self.identity),
                    );
                    tav.widget().set_vexpand(true);
                    widgets.sky_container.append(tav.widget());
                }
            }
        }

        // ── rebuild peer list when content changes ────────────────────────────

        if self.peer_list_generation != widgets.peer_list_shown_gen {
            rebuild_peer_list(widgets, self, &sender);
            widgets.peer_list_shown_gen = self.peer_list_generation;
            widgets.peers_page.set_needs_attention(!self.discovered_peers.is_empty());
        }

        // ── push peer pages for OpenPeer requests ─────────────────────────────

        let pending: Vec<PeerId> = self.pending_push_queue.borrow_mut().drain(..).collect();
        for peer_id in pending {
            if let Some(their_blob) = self.connected_peers.get(&peer_id) {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                // Only push if not already on the navigation stack.
                if widgets.peers_nav.find_page(&tag).is_none() {
                    if let Some(chart) = &self.chart {
                        let nickname = self.peer_nicknames.get(&tag).map(|s| s.as_str());
                        let (page, msg_list) = peer_page::build_peer_page(
                            &peer_id, their_blob, chart,
                            Rc::clone(&self.store),
                            Rc::clone(&self.identity),
                            &sender,
                            nickname,
                        );
                        page.set_tag(Some(&tag));
                        widgets.peers_nav.push(&page);
                        widgets.peer_msg_lists.insert(tag, msg_list);
                    }
                }
            } else {
                // Not connected yet — re-queue, will be retried on next update.
                self.pending_push_queue.borrow_mut().push(peer_id);
            }
        }

        // ── append new chat messages to peer message lists ────────────────────

        for (peer_id, messages) in &self.chat_logs {
            let tag = hex::encode_upper(&peer_id.0[..4]);
            let shown = widgets.peer_chat_shown.get(&tag).copied().unwrap_or(0);
            if messages.len() > shown {
                if let Some(list) = widgets.peer_msg_lists.get(&tag) {
                    for (from_us, text) in &messages[shown..] {
                        append_chat_row(list, text, *from_us);
                    }
                    widgets.peer_chat_shown.insert(tag, messages.len());
                }
            }
        }

        // ── network status button ─────────────────────────────────────────────

        {
            let connected = self.connected_peers.len();
            let online    = self.connected_channels.len();
            let node_line = if self.node_id_text.is_empty() {
                "Not connected".to_string()
            } else {
                format!("Node ···{}", self.node_id_text)
            };
            let text = format!(
                "{node_line}\n{connected} peer{} connected  ·  {online} online",
                if connected == 1 { "" } else { "s" },
            );
            widgets.net_popover_label.set_text(&text);
            let connected_any = !self.node_id_text.is_empty();
            widgets.net_status_btn.set_icon_name("network-wireless-symbolic");
            if connected_any {
                widgets.net_status_btn.remove_css_class("dim-label");
            } else {
                widgets.net_status_btn.add_css_class("dim-label");
            }
        }

        // ── notification bell ─────────────────────────────────────────────────

        {
            let total_unread: usize = self.unread_messages.values().sum();
            widgets.notif_btn.set_visible(total_unread > 0);
            if total_unread > 0 {
                let lines: String = self.unread_messages.iter()
                    .filter(|(_, &n)| n > 0)
                    .map(|(tag, n)| {
                        let name = self.peer_nicknames.get(tag)
                            .cloned()
                            .unwrap_or_else(|| format!("···{tag}"));
                        format!("{name}  ·  {n} unread")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                widgets.notif_label.set_text(&lines);
            }
        }

        // ── call bar ─────────────────────────────────────────────────────────

        match &self.call_state {
            CallState::Idle => {
                widgets.call_bar.set_visible(false);
            }
            CallState::Calling { peer_id } => {
                widgets.call_bar.set_visible(true);
                widgets.call_status.set_text(&format!(
                    "Calling ···{} …", hex::encode_upper(&peer_id.0[..4])
                ));
                widgets.accept_btn.set_visible(false);
                widgets.hangup_btn.set_visible(true);
            }
            CallState::Ringing { peer_id, .. } => {
                widgets.call_bar.set_visible(true);
                widgets.call_status.set_text(&format!(
                    "Incoming call from ···{}", hex::encode_upper(&peer_id.0[..4])
                ));
                widgets.accept_btn.set_visible(true);
                widgets.hangup_btn.set_visible(true);
            }
            CallState::Active { peer_id } => {
                widgets.call_bar.set_visible(true);
                widgets.call_status.set_text(&format!(
                    "In call with ···{}", hex::encode_upper(&peer_id.0[..4])
                ));
                widgets.accept_btn.set_visible(false);
                widgets.hangup_btn.set_visible(true);
            }
        }
    }
}

// ── peer list widget builder ──────────────────────────────────────────────────

/// Clear and rebuild the peer list groups inside `peers_content`.
fn rebuild_peer_list(
    widgets: &mut AppWidgets,
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) {
    // Remove all existing children.
    while let Some(child) = widgets.peers_content.first_child() {
        widgets.peers_content.remove(&child);
    }

    // ── Connected section ─────────────────────────────────────────────────────

    if !model.connected_peers.is_empty() {
        let group = adw::PreferencesGroup::new();
        group.set_title("Connected");

        let mut sorted: Vec<&PeerId> = model.connected_peers.keys().collect();
        sorted.sort_by_key(|id| hex::encode_upper(&id.0[..4]));

        for peer_id in sorted {
            let their_blob = &model.connected_peers[peer_id];
            let peer_hex = hex::encode_upper(&peer_id.0[..4]);
            let solar_month = zodia_core::solar_month(their_blob.birth.jdn);
            let glyph = sign_glyph(solar_month);
            let online = model.connected_channels.contains_key(peer_id);
            let display_name = model.peer_nicknames.get(&peer_hex)
                .cloned()
                .unwrap_or_else(|| format!("···{peer_hex}"));
            let unread = model.unread_messages.get(&peer_hex).copied().unwrap_or(0);

            let row = adw::ActionRow::new();
            row.set_title(&format!("{glyph}  {display_name}"));
            row.set_subtitle(if online { "● Online" } else { "○ Last seen" });
            if unread > 0 {
                let badge = gtk::Label::new(Some(&unread.to_string()));
                badge.add_css_class("badge");
                badge.add_css_class("accent");
                row.add_suffix(&badge);
            }
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.set_activatable(true);

            let pid = peer_id.clone();
            let s = sender.clone();
            row.connect_activated(move |_| s.input(AppMsg::OpenPeer(pid.clone())));
            group.add(&row);
        }
        widgets.peers_content.append(&group);
    }

    // ── Online section ────────────────────────────────────────────────────────

    let online: Vec<&DiscoveredPeer> = model.discovered_peers.iter()
        .filter(|p| !model.connected_peers.contains_key(&p.peer_id))
        .collect();

    let group = adw::PreferencesGroup::new();
    group.set_title("Online");

    if online.is_empty() && model.connected_peers.is_empty() {
        let status = adw::StatusPage::new();
        status.set_icon_name(Some("system-users-symbolic"));
        status.set_title("No peers nearby");
        status.set_description(Some(
            "Other Zodia users in your astrological neighbourhood will appear here as they come online.",
        ));
        widgets.peers_content.append(&status);
        return;
    } else if online.is_empty() {
        let row = adw::ActionRow::new();
        row.set_title("No other peers visible right now");
        group.add(&row);
    } else {
        let n = online.len();
        group.set_description(Some(&format!(
            "{n} peer{} nearby",
            if n == 1 { "" } else { "s" }
        )));
        for dp in &online {
            let glyph = sign_glyph(dp.solar_month);
            let aspects = if dp.approximate_aspects.is_empty() {
                "—".to_string()
            } else {
                dp.approximate_aspects.join("  ")
            };
            let row = adw::ActionRow::new();
            row.set_title(&format!("{glyph}  {aspects}"));
            row.set_subtitle(&dp.geohash_prefix);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.set_activatable(true);

            let pid = dp.peer_id.clone();
            let s = sender.clone();
            row.connect_activated(move |_| s.input(AppMsg::OpenPeer(pid.clone())));
            group.add(&row);
        }
    }
    widgets.peers_content.append(&group);
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn try_spawn_network(
    config: &LocalConfig,
    birth: &zodia_core::BirthData,
) -> Option<(ZodiaNetwork, Receiver<ZodiaNetEvent>)> {
    let signing_key = config.identity.signing_key().clone();
    match ZodiaNetwork::spawn(NetworkConfig { signing_key }, birth).await {
        Ok(pair) => Some(pair),
        Err(e) => { error!("network spawn failed: {e}"); None }
    }
}

fn start_network_command(
    sender: &AsyncComponentSender<AppModel>,
    rx: Receiver<ZodiaNetEvent>,
) {
    sender.command(|out, _shutdown| async move {
        let mut rx: Receiver<ZodiaNetEvent> = rx;
        while let Some(ev) = rx.recv().await {
            if out.send(ev).is_err() { break; }
        }
    });
}

fn make_tier1_blob(config: &LocalConfig) -> Option<Tier1Blob> {
    config.birth.as_ref().map(|birth| Tier1Blob {
        birth: birth.clone(),
        prekey:    [0u8; 32],
        ephemeral: [0u8; 32],
    })
}

// ── interpretation sync ───────────────────────────────────────────────────────

async fn do_interp_sync(
    channel: &DirectChannel,
    their_blob: &Tier1Blob,
    our_chart: Option<&Chart>,
    store: &Rc<RefCell<ZodiaStore>>,
    identity: &Rc<IdentityKeypair>,
    peer_hex: &str,
) {
    let outgoing = collect_entries_for_peer(their_blob, our_chart, store, identity);
    match channel.exchange_interps(&outgoing).await {
        Ok(received) => {
            let n = import_interps(&received, store, peer_hex);
            if n > 0 {
                info!(peer = %peer_hex, "imported {n} interpretations from peer");
            }
        }
        Err(e) => warn!(peer = %peer_hex, "interp sync failed: {e}"),
    }
}

fn collect_entries_for_peer(
    their_blob: &Tier1Blob,
    our_chart: Option<&Chart>,
    store: &Rc<RefCell<ZodiaStore>>,
    _identity: &Rc<IdentityKeypair>,
) -> Vec<InterpEntry> {
    let their_chart = Chart::compute(their_blob.birth.clone()).ok();
    let mut key_sigs: Vec<String> = Vec::new();

    if let Some(ref chart) = their_chart {
        for aspect in chart.natal_aspects() {
            key_sigs.push(InterpKey::from_natal(&aspect).to_sig());
        }
    }
    if let (Some(ref their_chart), Some(ours)) = (&their_chart, our_chart) {
        for aspect in compute_synastry(&ours.positions, &their_chart.positions) {
            key_sigs.push(InterpKey::from_synastry(&aspect).to_sig());
        }
    }

    let refs: Vec<&str> = key_sigs.iter().map(|s| s.as_str()).collect();
    store.borrow().community_for_keys(&refs, 100)
        .unwrap_or_default()
        .into_iter()
        .map(|e| InterpEntry {
            interp_key: e.interp_key,
            body: e.body,
            author_pk: e.author_pk,
            author_sig: e.author_sig.to_vec(),
        })
        .collect()
}

fn import_interps(
    entries: &[InterpEntry],
    store: &Rc<RefCell<ZodiaStore>>,
    peer_hex: &str,
) -> usize {
    let mut count = 0;
    for entry in entries {
        let Ok(sig_arr): Result<[u8; 64], _> = entry.author_sig.as_slice().try_into() else {
            warn!(peer = %peer_hex, key = %entry.interp_key, "invalid sig length, skipping");
            continue;
        };
        match store.borrow().insert_received(
            &entry.interp_key, &entry.body, &entry.author_pk, &sig_arr,
        ) {
            Ok(true)  => count += 1,
            Ok(false) => {}
            Err(StoreError::InvalidSignature) => {
                warn!(peer = %peer_hex, key = %entry.interp_key,
                      "received interpretation with invalid signature — discarded");
            }
            Err(e) => warn!(peer = %peer_hex, "interpretation import: {e}"),
        }
    }
    count
}

// ── session ID ────────────────────────────────────────────────────────────────

fn new_session_id(peer_id: &PeerId) -> [u8; 32] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&peer_id.0);
    hasher.update(&ts.to_le_bytes());
    *hasher.finalize().as_bytes()
}

// ── widget construction ───────────────────────────────────────────────────────

fn build_widgets(
    root: &adw::ApplicationWindow,
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) -> AppWidgets {
    root.set_default_size(800, 620);

    let outer_stack = gtk::Stack::new();
    outer_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    outer_stack.set_transition_duration(200);

    let (setup_page, setup_status) = build_setup_page(sender);
    outer_stack.add_named(&setup_page, Some("setup"));

    let (
        main_view,
        chart_container, sky_container,
        peers_nav, peers_content,
        peers_page,
        net_status_btn, net_popover_label,
        notif_btn, notif_label,
        call_bar, call_status, accept_btn, hangup_btn,
    ) = build_main_page(model, sender);
    outer_stack.add_named(&main_view, Some("main"));

    // Populate aspect views for returning users with an existing chart.
    if let Some(chart) = &model.chart {
        let nav = AspectView::natal(
            aspect_list::natal_items(&chart.natal_aspects()),
            chart,
            Rc::clone(&model.store),
            Rc::clone(&model.identity),
        );
        nav.widget().set_vexpand(true);
        chart_container.append(nav.widget());

        if let Ok(ts) = chart.transits_at(current_jdn()) {
            let tav = AspectView::transits(
                aspect_list::transit_items(&ts.transit_aspects, &ts.house_transits),
                Rc::clone(&model.store),
                Rc::clone(&model.identity),
            );
            tav.widget().set_vexpand(true);
            sky_container.append(tav.widget());
        }
    }

    outer_stack.set_visible_child_name(
        if model.on_setup_page { "setup" } else { "main" }
    );
    root.set_content(Some(&outer_stack));

    AppWidgets {
        outer_stack,
        setup_status,
        chart_container,
        sky_container,
        peers_nav,
        peers_content,
        peer_list_shown_gen: u64::MAX, // force initial build
        peer_msg_lists: HashMap::new(),
        peer_chat_shown: HashMap::new(),
        peers_page,
        net_status_btn,
        net_popover_label,
        notif_btn,
        notif_label,
        call_bar,
        call_status,
        accept_btn,
        hangup_btn,
    }
}

// ── setup page ────────────────────────────────────────────────────────────────

fn build_setup_page(
    sender: &AsyncComponentSender<AppModel>,
) -> (adw::ToolbarView, gtk::Label) {
    let toolbar_view = adw::ToolbarView::new();

    let header_bar = adw::HeaderBar::new();
    let title_label = gtk::Label::new(Some("Zodia"));
    title_label.add_css_class("title");
    header_bar.set_title_widget(Some(&title_label));
    toolbar_view.add_top_bar(&header_bar);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_vexpand(true);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(480);
    clamp.set_margin_top(24);
    clamp.set_margin_bottom(24);
    clamp.set_margin_start(12);
    clamp.set_margin_end(12);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_valign(gtk::Align::Center);
    content.set_vexpand(true);

    let title = gtk::Label::new(Some("Welcome to Zodia"));
    title.add_css_class("title-1");
    content.append(&title);

    let subtitle = gtk::Label::new(Some(
        "Enter your birth details to find your astrological neighbourhood.",
    ));
    subtitle.add_css_class("dim-label");
    subtitle.set_wrap(true);
    subtitle.set_max_width_chars(50);
    content.append(&subtitle);

    let date_group = adw::PreferencesGroup::new();
    date_group.set_title("Birth Date & Time");

    let year_row = adw::SpinRow::with_range(1900.0, 2100.0, 1.0);
    year_row.set_title("Year");
    year_row.set_value(1990.0);
    date_group.add(&year_row);

    let month_row = adw::SpinRow::with_range(1.0, 12.0, 1.0);
    month_row.set_title("Month");
    month_row.set_value(6.0);
    date_group.add(&month_row);

    let day_row = adw::SpinRow::with_range(1.0, 31.0, 1.0);
    day_row.set_title("Day");
    day_row.set_value(15.0);
    date_group.add(&day_row);

    let hour_row = adw::SpinRow::with_range(0.0, 23.0, 1.0);
    hour_row.set_title("Hour (UTC)");
    hour_row.set_value(12.0);
    date_group.add(&hour_row);

    let minute_row = adw::SpinRow::with_range(0.0, 59.0, 1.0);
    minute_row.set_title("Minute");
    minute_row.set_value(0.0);
    date_group.add(&minute_row);

    content.append(&date_group);

    let loc_group = adw::PreferencesGroup::new();
    loc_group.set_title("Birth Location");

    let lat_row = adw::EntryRow::new();
    lat_row.set_title("Latitude  (e.g. 51.5)");
    loc_group.add(&lat_row);

    let lon_row = adw::EntryRow::new();
    lon_row.set_title("Longitude  (e.g. -0.1)");
    loc_group.add(&lon_row);

    content.append(&loc_group);

    let setup_status = gtk::Label::new(None);
    setup_status.add_css_class("error");
    content.append(&setup_status);

    let btn = gtk::Button::with_label("Begin  →");
    btn.add_css_class("suggested-action");
    btn.add_css_class("pill");
    btn.set_halign(gtk::Align::Center);
    content.append(&btn);

    let s = sender.clone();
    let (yr, mr, dr, hr, minr, latr, lonr) = (
        year_row.clone(), month_row.clone(), day_row.clone(),
        hour_row.clone(), minute_row.clone(),
        lat_row.clone(), lon_row.clone(),
    );
    btn.connect_clicked(move |_| {
        let lat = match latr.text().parse::<f64>() {
            Ok(v) => v,
            Err(_) => { s.input(AppMsg::SetupError("Invalid latitude".into())); return; }
        };
        let lon = match lonr.text().parse::<f64>() {
            Ok(v) => v,
            Err(_) => { s.input(AppMsg::SetupError("Invalid longitude".into())); return; }
        };
        s.input(AppMsg::ConfirmBirth {
            year:   yr.value() as i32,
            month:  mr.value() as u32,
            day:    dr.value() as u32,
            hour:   hr.value() as u32,
            minute: minr.value() as u32,
            lat, lon,
        });
    });

    clamp.set_child(Some(&content));
    scroll.set_child(Some(&clamp));
    toolbar_view.set_content(Some(&scroll));

    (toolbar_view, setup_status)
}

// ── main page ─────────────────────────────────────────────────────────────────

#[allow(deprecated)] // ViewSwitcherTitle deprecated in ADW 1.4; migrate when bindings catch up
#[allow(clippy::type_complexity)]
fn build_main_page(
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) -> (
    adw::ToolbarView,
    gtk::Box, gtk::Box,                              // chart_container, sky_container
    adw::NavigationView, gtk::Box,                   // peers_nav, peers_content
    adw::ViewStackPage,                              // peers_page (for badge)
    gtk::MenuButton, gtk::Label,                     // net_status_btn, net_popover_label
    gtk::MenuButton, gtk::Label,                     // notif_btn, notif_label
    gtk::Box, gtk::Label, gtk::Button, gtk::Button,  // call bar
) {
    let toolbar_view = adw::ToolbarView::new();
    let view_stack = adw::ViewStack::new();

    // ── Chart tab ─────────────────────────────────────────────────────────────
    let chart_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    chart_container.set_vexpand(true);
    let chart_page = view_stack.add_titled(&chart_container, Some("chart"), "Chart");
    chart_page.set_icon_name(Some("weather-clear-symbolic"));
    let _ = chart_page;

    // ── Sky tab ───────────────────────────────────────────────────────────────
    let sky_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sky_container.set_vexpand(true);
    let sky_page = view_stack.add_titled(&sky_container, Some("sky"), "Sky");
    sky_page.set_icon_name(Some("night-light-symbolic"));
    let _ = sky_page;

    // ── Peers tab — has its own NavigationView for peer detail pages ──────────
    let peers_nav = adw::NavigationView::new();
    peers_nav.set_vexpand(true);

    // Root page: a scrolled box rebuilt dynamically as peers come and go.
    let peers_scroll = gtk::ScrolledWindow::new();
    peers_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    peers_scroll.set_vexpand(true);

    let peers_clamp = adw::Clamp::new();
    peers_clamp.set_maximum_size(720);
    peers_clamp.set_margin_top(8);
    peers_clamp.set_margin_bottom(8);
    peers_clamp.set_margin_start(12);
    peers_clamp.set_margin_end(12);

    let peers_content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    peers_clamp.set_child(Some(&peers_content));
    peers_scroll.set_child(Some(&peers_clamp));

    let peers_root = adw::NavigationPage::new(&peers_scroll, "Peers");
    peers_root.set_tag(Some("peers-root"));
    peers_nav.push(&peers_root);

    let peers_page = view_stack.add_titled(&peers_nav, Some("peers"), "Peers");
    peers_page.set_icon_name(Some("system-users-symbolic"));

    toolbar_view.set_content(Some(&view_stack));

    // ── Header bar ────────────────────────────────────────────────────────────
    let switcher_title = adw::ViewSwitcherTitle::new();
    switcher_title.set_stack(Some(&view_stack));
    switcher_title.set_title("Zodia");

    // Network status button — popover with node ID + peer counts.
    let net_popover_label = gtk::Label::new(Some("Not connected"));
    net_popover_label.set_margin_top(8);
    net_popover_label.set_margin_bottom(8);
    net_popover_label.set_margin_start(12);
    net_popover_label.set_margin_end(12);
    net_popover_label.add_css_class("dim-label");
    let net_popover = gtk::Popover::new();
    net_popover.set_child(Some(&net_popover_label));
    let net_status_btn = gtk::MenuButton::new();
    net_status_btn.set_icon_name("network-wireless-symbolic");
    net_status_btn.set_popover(Some(&net_popover));
    net_status_btn.set_tooltip_text(Some("Network status"));

    // Notification bell — only visible when there are unread messages.
    let notif_label = gtk::Label::new(None);
    notif_label.set_margin_top(8);
    notif_label.set_margin_bottom(8);
    notif_label.set_margin_start(12);
    notif_label.set_margin_end(12);
    let notif_popover = gtk::Popover::new();
    notif_popover.set_child(Some(&notif_label));
    let notif_btn = gtk::MenuButton::new();
    notif_btn.set_icon_name("notification-symbolic");
    notif_btn.set_popover(Some(&notif_popover));
    notif_btn.set_tooltip_text(Some("Notifications"));
    notif_btn.set_visible(false);

    let header_bar = adw::HeaderBar::new();
    header_bar.set_title_widget(Some(&switcher_title));
    header_bar.pack_end(&net_status_btn);
    header_bar.pack_end(&notif_btn);
    toolbar_view.add_top_bar(&header_bar);

    // ── Bottom bars ───────────────────────────────────────────────────────────
    let switcher_bar = adw::ViewSwitcherBar::new();
    switcher_bar.set_stack(Some(&view_stack));
    switcher_title
        .bind_property("title-visible", &switcher_bar, "reveal")
        .sync_create()
        .build();
    toolbar_view.add_bottom_bar(&switcher_bar);

    let call_bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    call_bar.add_css_class("toolbar");
    call_bar.set_margin_start(8);
    call_bar.set_margin_end(8);
    call_bar.set_visible(false);

    let call_status = gtk::Label::new(None);
    call_status.set_hexpand(true);
    call_status.set_halign(gtk::Align::Start);
    call_bar.append(&call_status);

    let accept_btn = gtk::Button::with_label("Accept  ✓");
    accept_btn.add_css_class("suggested-action");
    accept_btn.add_css_class("pill");
    accept_btn.set_visible(false);
    let s = sender.clone();
    accept_btn.connect_clicked(move |_| { s.input(AppMsg::AcceptCall); });
    call_bar.append(&accept_btn);

    let hangup_btn = gtk::Button::with_label("Hang up  ✕");
    hangup_btn.add_css_class("destructive-action");
    hangup_btn.add_css_class("pill");
    let s = sender.clone();
    hangup_btn.connect_clicked(move |_| { s.input(AppMsg::HangUp); });
    call_bar.append(&hangup_btn);

    toolbar_view.add_bottom_bar(&call_bar);

    let _ = model;

    (
        toolbar_view,
        chart_container, sky_container,
        peers_nav, peers_content,
        peers_page,
        net_status_btn, net_popover_label,
        notif_btn, notif_label,
        call_bar, call_status, accept_btn, hangup_btn,
    )
}

// ── nickname persistence ──────────────────────────────────────────────────────

fn load_nicknames(data_dir: &std::path::Path) -> HashMap<String, String> {
    let Ok(content) = std::fs::read_to_string(data_dir.join("nicknames.tsv")) else {
        return HashMap::new();
    };
    content.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let k = parts.next()?.to_string();
            let v = parts.next()?.to_string();
            Some((k, v))
        })
        .collect()
}

fn save_nicknames(data_dir: &std::path::Path, nicknames: &HashMap<String, String>) {
    let content: String = nicknames.iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| format!("{k}\t{v}\n"))
        .collect();
    let _ = std::fs::write(data_dir.join("nicknames.tsv"), content);
}
