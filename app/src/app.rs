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
use libadwaita::glib;
use libadwaita::prelude::*;
use relm4::prelude::*;
use tokio::sync::mpsc::Receiver;
use tracing::{debug, error, info, warn};
use zodia_av::AudioSession;
use zodia_config::LocalConfig;
use chrono::{NaiveDateTime, TimeZone as _, Timelike as _};
use zodia_core::{birth_from_coords, compute_synastry, current_jdn, gregorian_to_jdn,
                 Chart, InterpKey};
use zodia_crypto::IdentityKeypair;
use zodia_net::{ChannelMsg, DirectChannel, InterpEntry, NetworkConfig, PeerId, PeerStatus,
                Tier1Blob, ZodiaNetEvent, ZodiaNetwork};
use zodia_store::{StoreError, ZodiaStore};
use zodia_sync::{ReceivedInterp, ZodiaSyncNode};

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
    /// "+" pressed in the Network view — connect and add to sidebar, no navigation.
    ConnectPeer(PeerId),
    /// Sent internally after the network starts to force an initial update_view.
    NetworkReady,
    /// Re-publish our Tier-0 announce blob and reschedule the next announce.
    ReAnnounce,
    /// Retry a Tier-1 connection to a peer whose channel has dropped.
    Reconnect(PeerId),
    /// App window is closing — send Away to all connected peers.
    GoingOffline,
    /// User submitted a new interpretation — broadcast it to all live peers.
    ShareInterp(InterpEntry),
    /// A new community interpretation arrived via p2panda LogSync.
    SyncInterpReceived(ReceivedInterp),
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
    /// Active QUIC channels — presence means the channel is open.
    connected_channels: HashMap<PeerId, DirectChannel>,
    /// Explicit presence state received from each peer over their channel.
    peer_status: HashMap<PeerId, PeerStatus>,

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

    /// Channel to the background LogSync task for publishing new interpretations.
    /// `None` until the network is up.
    sync_publish_tx: Option<tokio::sync::mpsc::Sender<SyncPublishMsg>>,
}

// ── widgets ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct AppWidgets {
    outer_stack: gtk::Stack,
    setup_status: gtk::Label,

    chart_container: gtk::Box,
    sky_container: gtk::Box,

    /// Sidebar + content split layout.
    split_view: adw::OverlaySplitView,
    /// Single nav ListBox (Chart / Sky / Network / peers) — one selection source.
    nav_list: gtk::ListBox,
    /// Generation of the peer list we last rendered.
    peer_list_shown_gen: u64,

    /// Single content stack — chart / sky / network + peer pages, all as named children.
    content_stack: gtk::Stack,
    /// The "Network" scrollable view (rebuilt for discovered/online peers).
    peers_content: gtk::Box,

    /// Message list widget per peer (keyed by 4-byte hex tag).
    peer_msg_lists: HashMap<String, gtk::ListBox>,
    /// How many messages from `chat_logs` have already been appended to each list.
    peer_chat_shown: HashMap<String, usize>,
    /// Call and send buttons per peer — disabled when peer is offline.
    peer_actions: HashMap<String, (gtk::Button, gtk::Button, gtk::Entry)>,
    /// ViewSwitcherTitle per peer — updated when the nickname changes.
    #[allow(deprecated)]
    peer_titles: HashMap<String, adw::ViewSwitcherTitle>,

    /// Bell button — only visible when there are unread messages.
    notif_btn: gtk::MenuButton,
    /// Label inside the notification popover.
    notif_label: gtk::Label,
    /// Network status label shown in the Network content view header row.
    net_status_label: gtk::Label,

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
        let persisted_peers = load_peers(init.config.data_dir());

        // Pre-load chat history for all persisted peers.
        let chat_logs: HashMap<PeerId, Vec<(bool, String)>> = persisted_peers
            .keys()
            .filter_map(|peer_id| {
                let msgs = store.borrow().messages_for_peer(&peer_id.0).ok()?;
                if msgs.is_empty() { None } else { Some((peer_id.clone(), msgs)) }
            })
            .collect();

        let mut model = AppModel {
            on_setup_page: !has_birth,
            chart: None,
            store,
            network: None,
            node_id_text: String::new(),
            discovered_peers: Vec::new(),
            connected_peers: persisted_peers,
            connected_channels: HashMap::new(),
            peer_status: HashMap::new(),
            peer_list_generation: 0,
            pending_push_queue: RefCell::new(Vec::new()),
            config: init.config,
            setup_error: String::new(),
            identity,
            call_state: CallState::Idle,
            active_audio: None,
            chat_logs,
            peer_nicknames,
            unread_messages: HashMap::new(),
            sync_publish_tx: None,
        };

        if let Some(birth) = model.config.birth.clone() {
            if let Ok(chart) = Chart::compute(birth.clone()) {
                model.chart = Some(chart);
            }
        }

        let widgets = build_widgets(&root, &model, &sender);

        // Send Away to all connected peers when the window is closed.
        {
            let s = sender.clone();
            root.connect_close_request(move |_| {
                s.input(AppMsg::GoingOffline);
                glib::Propagation::Proceed
            });
        }

        if let Some(birth) = model.config.birth.clone() {
            if let Some((net, rx)) = try_spawn_network(&model.config, &birth).await {
                model.node_id_text = {
                    let nid = net.node_id();
                    hex::encode_upper(&nid.0[..4])
                };
                info!("network up, node ···{}", model.node_id_text);
                let _ = net.publish_announce().await;
                model.sync_publish_tx = try_spawn_sync(&model.config, &net, &sender).await;
                model.network = Some(net);
                start_network_command(&sender, rx);
                sender.input(AppMsg::NetworkReady);
                // Kick off periodic re-announce loop starting in 60 s.
                let s2 = sender.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    s2.input(AppMsg::ReAnnounce);
                });
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
                // Convert local birth time → UTC using the birth location's
                // IANA timezone (tzf-rs embeds the full boundary database).
                let utc_hour_frac = local_to_utc_hour(year, month, day, hour, minute, lat, lon);
                let jdn = gregorian_to_jdn(year, month, day, utc_hour_frac);
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
                    self.sync_publish_tx = try_spawn_sync(&self.config, &net, &sender).await;
                    self.network = Some(net);
                    start_network_command(&sender, rx);
                    sender.input(AppMsg::NetworkReady);
                    let s2 = sender.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                        s2.input(AppMsg::ReAnnounce);
                    });
                }
                self.setup_error.clear();
                self.on_setup_page = false;
            }

            AppMsg::SetupError(msg) => {
                self.setup_error = msg;
            }

            AppMsg::NetworkReady => {
                // After a short settle delay, attempt to reconnect every persisted
                // peer that we don't already have an active channel for.
                let peer_ids: Vec<PeerId> = self.connected_peers.keys().cloned().collect();
                if !peer_ids.is_empty() {
                    let s = sender.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(4)).await;
                        for peer_id in peer_ids {
                            s.input(AppMsg::Reconnect(peer_id));
                        }
                    });
                }
            }

            AppMsg::ReAnnounce => {
                if let Some(net) = &self.network {
                    if let Err(e) = net.publish_announce().await {
                        warn!("re-announce failed: {e}");
                    }
                }
                // Schedule the next announce in 60 s.
                let s = sender.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    s.input(AppMsg::ReAnnounce);
                });
            }

            AppMsg::GoingOffline => {
                for channel in self.connected_channels.values() {
                    let ch = channel.clone();
                    tokio::spawn(async move {
                        let _ = ch.send_msg(&ChannelMsg::StatusUpdate { status: PeerStatus::Away }).await;
                    });
                }
            }

            AppMsg::Reconnect(peer_id) => {
                // Only reconnect if we know the peer but no longer have a channel.
                if !self.connected_peers.contains_key(&peer_id)
                    || self.connected_channels.contains_key(&peer_id)
                {
                    return;
                }
                if let Some(net) = &self.network {
                    let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                    info!(peer = %peer_hex, "attempting auto-reconnect");
                    match net.connect_peer(&peer_id).await {
                        Ok(channel) => {
                            if let Some(our_blob) = make_tier1_blob(&self.config) {
                                match channel.exchange_tier1(&our_blob).await {
                                    Ok(their_blob) => {
                                        info!(peer = %peer_hex, "auto-reconnect tier-1 exchange ok");
                                        self.connected_peers.insert(peer_id.clone(), their_blob);
                                        self.peer_list_generation += 1;
                                    }
                                    Err(e) => warn!("auto-reconnect tier-1 exchange: {e}"),
                                }
                            }
                            net.accept_channel(peer_id.clone(), channel.clone());
                            send_status_active(&channel);
                            self.connected_channels.insert(peer_id, channel);
                        }
                        Err(e) => warn!(peer = %peer_hex, "auto-reconnect failed: {e}"),
                    }
                }
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

            AppMsg::ConnectPeer(peer_id) => {
                // "+" from Network view — establish Tier-1, add to sidebar, no navigation.
                if self.connected_peers.contains_key(&peer_id) {
                    return; // already added
                }
                if let Some(net) = &self.network {
                    let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                    match net.connect_peer(&peer_id).await {
                        Ok(channel) => {
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
                                        save_peers(self.config.data_dir(), &self.connected_peers);
                                        self.peer_list_generation += 1;
                                    }
                                    Err(e) => warn!("tier-1 exchange: {e}"),
                                }
                            }
                            net.accept_channel(peer_id.clone(), channel.clone());
                            send_status_active(&channel);
                            self.connected_channels.insert(peer_id, channel);
                        }
                        Err(e) => error!("connect_peer: {e}"),
                    }
                }
            }

            AppMsg::OpenPeer(peer_id) => {
                // Sidebar tap — navigate to peer page; connect first if needed.
                let tag = hex::encode_upper(&peer_id.0[..4]);
                self.unread_messages.remove(&tag);

                if !self.connected_peers.contains_key(&peer_id) {
                    // Not connected yet — connect silently, then queue navigation.
                    if let Some(net) = &self.network {
                        let peer_hex = tag.clone();
                        match net.connect_peer(&peer_id).await {
                            Ok(channel) => {
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
                                            save_peers(self.config.data_dir(), &self.connected_peers);
                                            self.peer_list_generation += 1;
                                        }
                                        Err(e) => warn!("tier-1 exchange: {e}"),
                                    }
                                }
                                net.accept_channel(peer_id.clone(), channel.clone());
                                send_status_active(&channel);
                                self.connected_channels.insert(peer_id.clone(), channel);
                            }
                            Err(e) => { error!("connect_peer: {e}"); return; }
                        }
                    }
                }

                // Queue a navigation push — fulfilled in update_view.
                self.pending_push_queue.borrow_mut().push(peer_id);
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
                        let _ = self.store.borrow().insert_message(&peer_id.0, true, &text);
                        self.chat_logs.entry(peer_id).or_default().push((true, text));
                    }
                }
            }
            AppMsg::ShareInterp(entry) => {
                // Fast path: send directly to already-connected peers.
                let msg = ChannelMsg::InterpShare { entries: vec![entry.clone()] };
                for (peer_id, channel) in &self.connected_channels {
                    let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                    if let Err(e) = channel.send_msg(&msg).await {
                        warn!(peer = %peer_hex, "interp share failed: {e}");
                    }
                }
                // Slow path: publish to the p2panda log for offline catch-up sync.
                if let Some(tx) = &self.sync_publish_tx {
                    if entry.author_sig.len() == 64 {
                        let mut sig = [0u8; 64];
                        sig.copy_from_slice(&entry.author_sig);
                        let _ = tx.try_send(SyncPublishMsg::Publish {
                            interp_key: entry.interp_key,
                            body: entry.body,
                            author_sig: sig,
                        });
                    }
                }
            }
            AppMsg::SyncInterpReceived(interp) => {
                debug!(
                    key = %interp.interp_key,
                    author = %hex::encode(&interp.author_pk[..4]),
                    "new interpretation received via sync"
                );
                // The sync background task already called insert_received.
                // Nothing more to do here — the next time the user opens
                // an aspect view it will query the refreshed store.
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
                    send_status_active(&channel);
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
                let _ = self.store.borrow().insert_message(&from.0, false, &text);
                self.chat_logs.entry(from).or_default().push((false, text));
            }
            ZodiaNetEvent::PeerStatusChanged { peer_id, status } => {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                info!(peer = %tag, ?status, "peer status update");
                self.peer_status.insert(peer_id, status);
                self.peer_list_generation += 1;
            }
            ZodiaNetEvent::PeerChannelClosed { peer_id } => {
                self.peer_status.remove(&peer_id);
                self.connected_channels.remove(&peer_id);
                self.peer_list_generation += 1;
                // If we have a Tier-1 relationship with this peer, schedule a
                // reconnect attempt after 10 s to restore the channel.
                if self.connected_peers.contains_key(&peer_id) {
                    let s = _sender.clone();
                    let pid = peer_id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        s.input(AppMsg::Reconnect(pid));
                    });
                }
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
                    sender.clone(),
                );
                nav.widget().set_vexpand(true);
                widgets.chart_container.append(nav.widget());

                if let Ok(ts) = chart.transits_at(current_jdn()) {
                    let tav = AspectView::transits(
                        aspect_list::transit_items(&ts.transit_aspects, &ts.house_transits),
                        Rc::clone(&self.store),
                        Rc::clone(&self.identity),
                        sender.clone(),
                    );
                    tav.widget().set_vexpand(true);
                    widgets.sky_container.append(tav.widget());
                }
            }
        }

        // ── rebuild peer list when content changes ────────────────────────────

        if self.peer_list_generation != widgets.peer_list_shown_gen {
            rebuild_sidebar_peers(widgets, self, &sender);
            rebuild_network_view(widgets, self, &sender);
            widgets.peer_list_shown_gen = self.peer_list_generation;
        }

        // ── push peer pages for OpenPeer requests ─────────────────────────────

        let pending: Vec<PeerId> = self.pending_push_queue.borrow_mut().drain(..).collect();
        for peer_id in pending {
            if let Some(their_blob) = self.connected_peers.get(&peer_id) {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                if let Some(chart) = &self.chart {
                    let nickname = self.peer_nicknames.get(&tag).map(|s| s.as_str());
                    if widgets.content_stack.child_by_name(&tag).is_some() {
                        // Page already built — switch to it directly.
                        widgets.content_stack.set_visible_child_name(&tag);
                    } else {
                        let (toolbar_view, msg_list, call_btn, send_btn, entry, switcher_title) =
                            peer_page::build_peer_page(
                                &peer_id, their_blob, chart,
                                Rc::clone(&self.store),
                                Rc::clone(&self.identity),
                                &sender,
                                nickname,
                                &widgets.split_view,
                            );
                        let online = self.connected_channels.contains_key(&peer_id);
                        call_btn.set_sensitive(online);
                        send_btn.set_sensitive(online);
                        entry.set_sensitive(online);
                        widgets.content_stack.add_named(&toolbar_view, Some(&tag));
                        widgets.content_stack.set_visible_child_name(&tag);
                        widgets.peer_msg_lists.insert(tag.clone(), msg_list);
                        widgets.peer_actions.insert(tag.clone(), (call_btn, send_btn, entry));
                        widgets.peer_titles.insert(tag, switcher_title);
                    }
                    if widgets.split_view.is_collapsed() {
                        widgets.split_view.set_show_sidebar(false);
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

        // ── update call/send button sensitivity for open peer pages ──────────

        for (tag, (call_btn, send_btn, entry)) in &widgets.peer_actions {
            let online = self.connected_channels.keys()
                .any(|id| hex::encode_upper(&id.0[..4]) == *tag);
            call_btn.set_sensitive(online);
            send_btn.set_sensitive(online);
            entry.set_sensitive(online);
        }

        // ── network status label (shown in the Network content view) ─────────

        {
            let connected = self.connected_peers.len();
            let active    = self.peer_status.values()
                .filter(|s| **s == PeerStatus::Active).count();
            let text = if self.node_id_text.is_empty() {
                "Starting up…".to_string()
            } else if connected == 0 {
                format!("Node ···{}  ·  searching…", self.node_id_text)
            } else {
                format!("Node ···{}  ·  {} people  ·  {} online",
                        self.node_id_text, connected, active)
            };
            widgets.net_status_label.set_text(&text);
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

// ── sidebar peer list builder ─────────────────────────────────────────────────

/// Rebuild the peer rows in `nav_list` (indices 4+) and refresh any open peer
/// page titles so nickname changes are reflected immediately.
#[allow(deprecated)] // ViewSwitcherTitle
fn rebuild_sidebar_peers(
    widgets: &mut AppWidgets,
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) {
    // Remove all rows at index >= 4 (peer rows from last build).
    let mut to_remove: Vec<gtk::ListBoxRow> = Vec::new();
    let mut idx = 4i32;
    while let Some(row) = widgets.nav_list.row_at_index(idx) {
        to_remove.push(row);
        idx += 1;
    }
    for row in to_remove {
        widgets.nav_list.remove(&row);
    }

    let mut sorted: Vec<&PeerId> = model.connected_peers.keys().collect();
    sorted.sort_by_key(|id| hex::encode_upper(&id.0[..4]));

    for peer_id in sorted {
        let their_blob   = &model.connected_peers[peer_id];
        let peer_hex     = hex::encode_upper(&peer_id.0[..4]);
        let solar_month  = zodia_core::solar_month(their_blob.birth.jdn);
        let glyph        = sign_glyph(solar_month);
        let status       = model.peer_status.get(peer_id);
        let has_channel  = model.connected_channels.contains_key(peer_id);
        let display_name = model.peer_nicknames.get(&peer_hex)
            .cloned()
            .unwrap_or_else(|| format!("···{peer_hex}"));
        let unread = model.unread_messages.get(&peer_hex).copied().unwrap_or(0);

        let row = gtk::ListBoxRow::new();
        // Store full peer id hex as widget name for activation lookup.
        row.set_widget_name(&hex::encode(&peer_id.0));

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);

        // Three-state presence dot — drawn as a 6 px circle so it sits at the
        // true geometric centre of the row rather than riding the text baseline.
        let (dot_filled, dot_color) = match (has_channel, status) {
            (_, Some(PeerStatus::Active))  => (true,  [0.46_f32, 0.82, 0.46, 1.0]),
            (_, Some(PeerStatus::Away))    => (true,  [0.95,     0.75, 0.30, 1.0]),
            (true,  None)                  => (true,  [0.95,     0.75, 0.30, 1.0]),
            (false, _)                     => (false, [0.55,     0.55, 0.55, 0.7]),
        };
        let dot = gtk::DrawingArea::new();
        dot.set_size_request(8, 8);
        dot.set_valign(gtk::Align::Center);
        dot.set_draw_func(move |_, cr, w, h| {
            let (r, g, b, a) = (dot_color[0] as f64, dot_color[1] as f64,
                                dot_color[2] as f64, dot_color[3] as f64);
            let cx = w as f64 / 2.0;
            let cy = h as f64 / 2.0;
            let radius = (w.min(h)) as f64 / 2.0;
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            if dot_filled {
                cr.set_source_rgba(r, g, b, a);
                let _ = cr.fill();
            } else {
                cr.set_source_rgba(r, g, b, a);
                cr.set_line_width(1.2);
                let _ = cr.stroke();
            }
        });
        hbox.append(&dot);

        let lbl = gtk::Label::new(Some(&format!("{glyph}  {display_name}")));
        lbl.set_halign(gtk::Align::Start);
        lbl.set_hexpand(true);
        hbox.append(&lbl);

        if unread > 0 {
            let badge = gtk::Label::new(Some(&unread.to_string()));
            badge.add_css_class("badge");
            badge.add_css_class("accent");
            badge.set_valign(gtk::Align::Center);
            hbox.append(&badge);
        }

        // Pencil icon — transparent until hover, always reserves space so the
        // row height never jumps. Plain Image + GestureClick avoids button
        // padding that would make peer rows taller than chart/sky/network rows.
        let edit_img = gtk::Image::from_icon_name("document-edit-symbolic");
        edit_img.set_pixel_size(16);
        edit_img.set_opacity(0.0);
        edit_img.set_valign(gtk::Align::Center);
        edit_img.set_tooltip_text(Some("Set nickname"));
        hbox.append(&edit_img);

        // Row hover → dim (0.4 opacity); icon hover → full (1.0 opacity).
        let motion_row = gtk::EventControllerMotion::new();
        let img_row_enter = edit_img.clone();
        let img_row_leave = edit_img.clone();
        motion_row.connect_enter(move |_, _, _| img_row_enter.set_opacity(0.4));
        motion_row.connect_leave(move |_| img_row_leave.set_opacity(0.0));
        row.add_controller(motion_row);

        let motion_icon = gtk::EventControllerMotion::new();
        let img_icon_enter = edit_img.clone();
        let img_icon_leave = edit_img.clone();
        motion_icon.connect_enter(move |_, _, _| img_icon_enter.set_opacity(1.0));
        motion_icon.connect_leave(move |_| img_icon_leave.set_opacity(0.4));
        edit_img.add_controller(motion_icon);

        // Open a nickname dialog on click.
        {
            let pid     = peer_id.0;
            let s       = sender.clone();
            let current = model.peer_nicknames.get(&peer_hex).cloned().unwrap_or_default();
            let img_ref = edit_img.clone();
            let click   = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                let dialog = adw::AlertDialog::new(Some("Set Nickname"), None);
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("set", "Set");
                dialog.set_response_appearance("set", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("set"));
                dialog.set_close_response("cancel");

                let entry = gtk::Entry::new();
                entry.set_text(&current);
                entry.set_placeholder_text(Some("Nickname…"));
                dialog.set_extra_child(Some(&entry));

                let s2 = s.clone();
                let e  = entry.clone();
                dialog.connect_response(None, move |_, id| {
                    if id == "set" {
                        s2.input(AppMsg::SetNickname {
                            peer_id: PeerId(pid),
                            name: e.text().to_string(),
                        });
                    }
                });
                dialog.present(Some(&img_ref));
            });
            edit_img.add_controller(click);
        }

        row.set_child(Some(&hbox));
        widgets.nav_list.append(&row);

        // Keep the open peer page title in sync with the current nickname.
        if let Some(title_widget) = widgets.peer_titles.get(&peer_hex) {
            let title_text = model.peer_nicknames.get(&peer_hex)
                .filter(|n| !n.is_empty())
                .map(|n| format!("{glyph}  {n}"))
                .unwrap_or_else(|| format!("{glyph}  ···{peer_hex}"));
            title_widget.set_title(&title_text);
        }
    }
}

// ── network content view builder ──────────────────────────────────────────────

/// Rebuild the "Network" content pane — shows discoverable peers not yet connected.
/// The net_status_label at the top of peers_content is a persistent widget and
/// is NOT touched here; it is updated separately in update_view.
fn rebuild_network_view(
    widgets: &mut AppWidgets,
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) {
    // Remove everything except the first child (net_status_label).
    loop {
        match widgets.peers_content.last_child() {
            Some(child) if child != widgets.peers_content.first_child().unwrap() => {
                widgets.peers_content.remove(&child);
            }
            _ => break,
        }
    }

    let discoverable: Vec<&DiscoveredPeer> = model.discovered_peers.iter()
        .filter(|p| !model.connected_peers.contains_key(&p.peer_id))
        .collect();

    if discoverable.is_empty() {
        let status = adw::StatusPage::new();
        status.set_icon_name(Some("network-wireless-symbolic"));
        status.set_title("No peers found yet");
        status.set_description(Some(
            "Other Zodia users on the network will appear here as they are discovered.",
        ));
        widgets.peers_content.append(&status);
        return;
    }

    let group = adw::PreferencesGroup::new();
    let n = discoverable.len();
    group.set_title(&format!(
        "{n} peer{} on the network",
        if n == 1 { "" } else { "s" }
    ));

    for dp in &discoverable {
        let glyph = sign_glyph(dp.solar_month);
        let aspects = if dp.approximate_aspects.is_empty() {
            "—".to_string()
        } else {
            dp.approximate_aspects.join("  ")
        };
        let row = adw::ActionRow::new();
        row.set_title(&aspects);
        row.set_subtitle(&dp.geohash_prefix);

        // Glyph in a plain Label prefix so it goes through the same text
        // rendering path as the sidebar — not through Pango markup.
        let glyph_lbl = gtk::Label::new(Some(glyph));
        glyph_lbl.set_valign(gtk::Align::Center);
        row.add_prefix(&glyph_lbl);
        row.set_activatable(false);

        // "+" button — initiates Tier-1 connection; peer moves to sidebar on success.
        let add_btn = gtk::Button::new();
        add_btn.set_icon_name("list-add-symbolic");
        add_btn.add_css_class("flat");
        add_btn.set_valign(gtk::Align::Center);
        add_btn.set_tooltip_text(Some("Connect to this peer"));
        let pid = dp.peer_id.clone();
        let s = sender.clone();
        add_btn.connect_clicked(move |_| s.input(AppMsg::ConnectPeer(pid.clone())));
        row.add_suffix(&add_btn);

        group.add(&row);
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

/// Message type for sending publish requests to the background sync task.
pub(crate) enum SyncPublishMsg {
    Publish { interp_key: String, body: String, author_sig: [u8; 64] },
}

/// Spawn the LogSync background task and return a channel for publishing.
///
/// Opens a second connection to the same SQLite file so the sync task can
/// call `insert_received` without conflicting with the main-thread store
/// (WAL mode allows concurrent readers + one writer).
async fn try_spawn_sync(
    config: &LocalConfig,
    net: &ZodiaNetwork,
    sender: &AsyncComponentSender<AppModel>,
) -> Option<tokio::sync::mpsc::Sender<SyncPublishMsg>> {
    use std::sync::{Arc, Mutex};
    use zodia_core::topic_key_global;

    let store_path = config.data_dir().join("interpretations.db");
    let sync_store = match ZodiaStore::open(&store_path) {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => { warn!("sync store open failed: {e}"); return None; }
    };

    let panda_key = config.identity.to_panda_key();
    let topic = topic_key_global().0;

    let node = match ZodiaSyncNode::spawn(
        panda_key,
        net.endpoint(),
        net.gossip(),
        sync_store,
        topic,
    ).await {
        Ok(n) => n,
        Err(e) => { warn!("sync node spawn failed: {e}"); return None; }
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<SyncPublishMsg>(32);
    let sender_bg = sender.clone();

    tokio::spawn(async move {
        let mut node = node;
        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    match msg {
                        SyncPublishMsg::Publish { interp_key, body, author_sig } => {
                            if let Err(e) = node.publish(&interp_key, &body, &author_sig).await {
                                warn!("sync publish: {e}");
                            }
                        }
                    }
                }
                Some(interp) = node.received.recv() => {
                    sender_bg.input(AppMsg::SyncInterpReceived(interp));
                }
                else => break,
            }
        }
    });

    Some(tx)
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
        split_view, nav_list,
        content_stack, peers_content,
        notif_btn, notif_label,
        net_status_label,
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
            sender.clone(),
        );
        nav.widget().set_vexpand(true);
        chart_container.append(nav.widget());

        if let Ok(ts) = chart.transits_at(current_jdn()) {
            let tav = AspectView::transits(
                aspect_list::transit_items(&ts.transit_aspects, &ts.house_transits),
                Rc::clone(&model.store),
                Rc::clone(&model.identity),
                sender.clone(),
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
        split_view,
        nav_list,
        peer_list_shown_gen: u64::MAX, // force initial build
        content_stack,
        peers_content,
        peer_msg_lists: HashMap::new(),
        peer_chat_shown: HashMap::new(),
        peer_actions: HashMap::new(),
        peer_titles: HashMap::new(),
        notif_btn,
        notif_label,
        net_status_label,
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
    date_group.set_description(Some("Enter your local birth time — timezone is resolved automatically from the birth location."));

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
    hour_row.set_title("Hour (local)");
    hour_row.set_value(12.0);
    date_group.add(&hour_row);

    let minute_row = adw::SpinRow::with_range(0.0, 59.0, 1.0);
    minute_row.set_title("Minute");
    minute_row.set_value(0.0);
    date_group.add(&minute_row);

    content.append(&date_group);

    let loc_group = adw::PreferencesGroup::new();
    loc_group.set_title("Birth Location");

    let city_row = adw::EntryRow::new();
    city_row.set_title("City");
    loc_group.add(&city_row);

    let lat_row = adw::EntryRow::new();
    lat_row.set_title("Latitude  (e.g. 51.5)");
    loc_group.add(&lat_row);

    let lon_row = adw::EntryRow::new();
    lon_row.set_title("Longitude  (e.g. -0.1)");
    loc_group.add(&lon_row);

    content.append(&loc_group);

    // ── City autocomplete popover ─────────────────────────────────────────────
    let city_list = gtk::ListBox::new();
    city_list.set_selection_mode(gtk::SelectionMode::None);
    city_list.add_css_class("boxed-list");
    let city_scroll = gtk::ScrolledWindow::new();
    city_scroll.set_child(Some(&city_list));
    city_scroll.set_max_content_height(280);
    city_scroll.set_propagate_natural_height(true);
    city_scroll.set_min_content_width(280);

    let city_popover = gtk::Popover::new();
    city_popover.set_child(Some(&city_scroll));
    city_popover.set_position(gtk::PositionType::Bottom);
    city_popover.set_autohide(true);
    city_popover.set_has_arrow(false);
    city_popover.set_parent(&city_row);

    let city_hits: Rc<RefCell<Vec<zodia_core::CityHit>>> = Rc::new(RefCell::new(Vec::new()));

    {
        let hits = city_hits.clone();
        let lat_r = lat_row.clone();
        let lon_r = lon_row.clone();
        let pop   = city_popover.clone();
        city_list.connect_row_activated(move |_, row| {
            let idx = row.index() as usize;
            let guard = hits.borrow();
            if let Some(hit) = guard.get(idx) {
                lat_r.set_text(&format!("{:.4}", hit.lat));
                lon_r.set_text(&format!("{:.4}", hit.lon));
            }
            pop.popdown();
        });
    }
    {
        let hits = city_hits.clone();
        let list = city_list.clone();
        let pop  = city_popover.clone();
        city_row.connect_changed(move |entry| {
            let text = entry.text();
            let results = zodia_core::search_cities(text.as_str(), 10);
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            if results.is_empty() {
                pop.popdown();
                *hits.borrow_mut() = results;
                return;
            }
            for hit in &results {
                let lbl = gtk::Label::new(Some(&format!("{}, {}", hit.name, hit.country)));
                lbl.set_halign(gtk::Align::Start);
                lbl.set_margin_start(12);
                lbl.set_margin_end(12);
                lbl.set_margin_top(8);
                lbl.set_margin_bottom(8);
                list.append(&lbl);
            }
            *hits.borrow_mut() = results;
            pop.popup();
        });
    }

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

#[allow(clippy::type_complexity)]
fn build_main_page(
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) -> (
    adw::ToolbarView,                                   // outermost wrapper (call_bar bottom)
    gtk::Box, gtk::Box,                                 // chart_container, sky_container
    adw::OverlaySplitView, gtk::ListBox,                 // split_view, nav_list
    gtk::Stack, gtk::Box,                               // content_stack, peers_content
    gtk::MenuButton, gtk::Label,                        // notif_btn, notif_label
    gtk::Label,                                         // net_status_label
    gtk::Box, gtk::Label, gtk::Button, gtk::Button,     // call bar
) {
    // ── Notification bell (sidebar header) ───────────────────────────────────

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

    // ── Sidebar ───────────────────────────────────────────────────────────────

    // ── Nav list (fixed: Chart / Sky / Network) ───────────────────────────────
    let nav_list = gtk::ListBox::new();
    nav_list.add_css_class("navigation-sidebar");
    nav_list.set_selection_mode(gtk::SelectionMode::Single);

    let make_nav_row = |icon: &str, label_text: &str| -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);
        hbox.set_margin_top(10);
        hbox.set_margin_bottom(10);
        let img = gtk::Image::from_icon_name(icon);
        img.set_pixel_size(16);
        hbox.append(&img);
        let lbl = gtk::Label::new(Some(label_text));
        lbl.set_halign(gtk::Align::Start);
        hbox.append(&lbl);
        row.set_child(Some(&hbox));
        row
    };

    nav_list.append(&make_nav_row("weather-clear-symbolic",   "Chart"));
    nav_list.append(&make_nav_row("night-light-symbolic",     "Sky"));
    nav_list.append(&make_nav_row("network-wireless-symbolic","Network"));

    // ── Section header row (index 3) — not selectable, separates nav from peers ──
    {
        let header_row = gtk::ListBoxRow::new();
        let lbl = gtk::Label::new(Some("People"));
        lbl.add_css_class("heading");
        lbl.add_css_class("dim-label");
        lbl.set_halign(gtk::Align::Start);
        lbl.set_margin_start(12);
        lbl.set_margin_end(12);
        lbl.set_margin_top(12);
        lbl.set_margin_bottom(2);
        header_row.set_child(Some(&lbl));
        header_row.set_selectable(false);
        header_row.set_activatable(false);
        nav_list.append(&header_row);
    }
    // Peer rows are appended from index 4 onward by rebuild_sidebar_peers.

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.append(&nav_list);

    let sidebar_scroll = gtk::ScrolledWindow::new();
    sidebar_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    sidebar_scroll.set_vexpand(true);
    sidebar_scroll.set_child(Some(&sidebar_box));

    let sidebar_toolbar = adw::ToolbarView::new();
    let sidebar_header = adw::HeaderBar::new();
    let sidebar_title = adw::WindowTitle::new("Zodia", "");
    sidebar_header.set_title_widget(Some(&sidebar_title));
    sidebar_header.pack_end(&notif_btn);
    sidebar_toolbar.add_top_bar(&sidebar_header);
    sidebar_toolbar.set_content(Some(&sidebar_scroll));

    // ── Content area — single crossfade Stack for all views ──────────────────

    let content_stack = gtk::Stack::new();
    content_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    content_stack.set_transition_duration(150);

    // Sidebar toggle button — shown only when the split view is collapsed
    // (narrow window / mobile). Each content header gets one clone.
    let make_sidebar_btn = || {
        let btn = gtk::Button::from_icon_name("sidebar-show-symbolic");
        btn.set_tooltip_text(Some("Show sidebar"));
        btn.set_visible(false);
        btn
    };

    // Chart view
    let chart_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    chart_container.set_vexpand(true);
    let chart_header = adw::HeaderBar::new();
    chart_header.set_title_widget(Some(&adw::WindowTitle::new("Chart", "")));
    let chart_sidebar_btn = make_sidebar_btn();
    chart_header.pack_start(&chart_sidebar_btn);
    let chart_toolbar = adw::ToolbarView::new();
    chart_toolbar.add_top_bar(&chart_header);
    chart_toolbar.set_content(Some(&chart_container));
    content_stack.add_named(&chart_toolbar, Some("chart"));

    // Sky view
    let sky_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sky_container.set_vexpand(true);
    let sky_header = adw::HeaderBar::new();
    sky_header.set_title_widget(Some(&adw::WindowTitle::new("Sky", "")));
    let sky_sidebar_btn = make_sidebar_btn();
    sky_header.pack_start(&sky_sidebar_btn);
    let sky_toolbar = adw::ToolbarView::new();
    sky_toolbar.add_top_bar(&sky_header);
    sky_toolbar.set_content(Some(&sky_container));
    content_stack.add_named(&sky_toolbar, Some("sky"));

    // Network view
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

    let net_status_label = gtk::Label::new(Some("Starting up…"));
    net_status_label.add_css_class("dim-label");
    net_status_label.add_css_class("caption");
    net_status_label.set_halign(gtk::Align::Center);
    net_status_label.set_margin_top(8);
    peers_content.append(&net_status_label);

    peers_clamp.set_child(Some(&peers_content));
    peers_scroll.set_child(Some(&peers_clamp));

    let network_header = adw::HeaderBar::new();
    network_header.set_title_widget(Some(&adw::WindowTitle::new("Network", "")));
    let network_sidebar_btn = make_sidebar_btn();
    network_header.pack_start(&network_sidebar_btn);
    let network_toolbar = adw::ToolbarView::new();
    network_toolbar.add_top_bar(&network_header);
    network_toolbar.set_content(Some(&peers_scroll));
    content_stack.add_named(&network_toolbar, Some("network"));

    // Peer pages are added dynamically as named children when first opened.

    // ── Overlay split view ────────────────────────────────────────────────────

    let split_view = adw::OverlaySplitView::new();
    split_view.set_sidebar(Some(&sidebar_toolbar));
    split_view.set_content(Some(&content_stack));
    split_view.set_min_sidebar_width(200.0);
    split_view.set_max_sidebar_width(280.0);

    // Show/hide sidebar toggle buttons when the view collapses/expands.
    {
        let btns = [
            chart_sidebar_btn.clone(),
            sky_sidebar_btn.clone(),
            network_sidebar_btn.clone(),
        ];
        split_view.connect_notify_local(Some("collapsed"), move |sv, _| {
            let collapsed = sv.is_collapsed();
            for btn in &btns {
                btn.set_visible(collapsed);
            }
        });
        for btn in [chart_sidebar_btn, sky_sidebar_btn, network_sidebar_btn] {
            let sv = split_view.clone();
            btn.connect_clicked(move |_| sv.set_show_sidebar(true));
        }
    }

    // Default selection: Chart
    nav_list.select_row(nav_list.row_at_index(0).as_ref());

    // ── Wire up nav row activation ────────────────────────────────────────────

    {
        let cs = content_stack.clone();
        let sv = split_view.clone();
        let s  = sender.clone();

        nav_list.connect_row_activated(move |_, row| {
            match row.index() {
                0 | 1 | 2 => {
                    let page = match row.index() {
                        0 => "chart",
                        1 => "sky",
                        2 => "network",
                        _ => unreachable!(),
                    };
                    cs.set_visible_child_name(page);
                    if sv.is_collapsed() { sv.set_show_sidebar(false); }
                }
                idx if idx >= 4 => {
                    // Peer row — widget name holds the full peer id hex.
                    let name = row.widget_name();
                    if let Ok(bytes) = hex::decode(name.as_str()) {
                        if let Ok(arr) = bytes.try_into() as Result<[u8; 32], _> {
                            if sv.is_collapsed() { sv.set_show_sidebar(false); }
                            s.input(AppMsg::OpenPeer(PeerId(arr)));
                        }
                    }
                }
                _ => {} // header row at index 3 — not activatable
            }
        });
    }

    // ── Outer ToolbarView — just hosts the call bar at bottom ─────────────────

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.set_content(Some(&split_view));

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
        split_view, nav_list,
        content_stack, peers_content,
        notif_btn, notif_label,
        net_status_label,
        call_bar, call_status, accept_btn, hangup_btn,
    )
}

// ── status helpers ────────────────────────────────────────────────────────────

/// Fire-and-forget: send `Active` to a newly established channel.
fn send_status_active(channel: &DirectChannel) {
    let ch = channel.clone();
    tokio::spawn(async move {
        let _ = ch.send_msg(&ChannelMsg::StatusUpdate { status: PeerStatus::Active }).await;
    });
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

// ── peer persistence ──────────────────────────────────────────────────────────

/// Load previously connected peers from `peers.tsv`.
/// Format: `{peer_id_hex64}\t{jdn}\t{geohash}`
fn load_peers(data_dir: &std::path::Path) -> HashMap<PeerId, zodia_net::Tier1Blob> {
    let Ok(content) = std::fs::read_to_string(data_dir.join("peers.tsv")) else {
        return HashMap::new();
    };
    content.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let id_hex  = parts.next()?;
            let jdn: f64 = parts.next()?.parse().ok()?;
            let geohash  = parts.next()?.to_string();
            let id_bytes: Vec<u8> = hex::decode(id_hex).ok()?;
            let id_arr: [u8; 32] = id_bytes.try_into().ok()?;
            let peer_id = PeerId(id_arr);
            let blob = zodia_net::Tier1Blob {
                birth: zodia_core::BirthData::new(jdn, geohash),
                prekey:    [0u8; 32],
                ephemeral: [0u8; 32],
            };
            Some((peer_id, blob))
        })
        .collect()
}

fn save_peers(data_dir: &std::path::Path, peers: &HashMap<PeerId, zodia_net::Tier1Blob>) {
    let content: String = peers.iter()
        .map(|(id, blob)| format!("{}\t{}\t{}\n", hex::encode(&id.0), blob.birth.jdn, blob.birth.geohash))
        .collect();
    let _ = std::fs::write(data_dir.join("peers.tsv"), content);
}

/// Convert a local birth time to a UTC fractional hour using the IANA timezone
/// for the given coordinates.  Falls back to a naive UTC offset of lon/15 if
/// the timezone database lookup fails.
fn local_to_utc_hour(
    year: i32, month: u32, day: u32,
    hour: u32, minute: u32,
    lat: f64, lon: f64,
) -> f64 {
    use chrono_tz::Tz;

    let finder = tzf_rs::DefaultFinder::new();
    let tz_name = finder.get_tz_name(lon, lat);
    if let Ok(tz) = tz_name.parse::<Tz>() {
        if let Some(naive) = NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(year, month, day).unwrap_or_default(),
            chrono::NaiveTime::from_hms_opt(hour, minute, 0).unwrap_or_default(),
        ).checked_add_signed(chrono::Duration::zero()) {
            // Use the earliest valid UTC interpretation (handles ambiguous DST transitions).
            if let chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _)
                = tz.from_local_datetime(&naive)
            {
                let utc_dt = dt.with_timezone(&chrono::Utc);
                return utc_dt.hour() as f64 + utc_dt.minute() as f64 / 60.0
                    + utc_dt.second() as f64 / 3600.0;
            }
        }
    }
    // Fallback: crude solar time offset (±12 h)
    let offset_h = (lon / 15.0).round().clamp(-12.0, 14.0);
    let raw = hour as f64 + minute as f64 / 60.0 - offset_h;
    raw.rem_euclid(24.0)
}

fn save_nicknames(data_dir: &std::path::Path, nicknames: &HashMap<String, String>) {
    let content: String = nicknames.iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| format!("{k}\t{v}\n"))
        .collect();
    let _ = std::fs::write(data_dir.join("nicknames.tsv"), content);
}
