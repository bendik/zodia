//! Root application component.
//!
//! `AppModel` is an `AsyncComponent` that drives the full lifecycle:
//!   1. First-run setup  — collect birth date + location, compute chart
//!   2. Main view        — Chart / Sky / Peers tabs in an `adw::ToolbarView`
//!   3. Connected peer   — pushed onto the Peers tab's own `NavigationView`;
//!                         shows synastry + call interface
//!   4. Network events   — `CommandOutput = ZodiaNetEvent` keeps all three
//!                         tabs reactive without blocking the GTK thread

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

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
use zodia_crypto::{ecies_decrypt, ecies_encrypt};
use zodia_net::{ChannelMsg, ConsentBlob, DirectChannel, InterpEntry,
                NetworkConfig, PeerId, PeerStatus, RelayPayload, ZodiaNetEvent, ZodiaNetwork};
use zodia_store::{StoreError, ZodiaStore, BaselineStore};
use zodia_sync::{ReceivedInterp, ZodiaSyncNode};

use crate::aspect_list;
use crate::aspect_view::AspectView;
use crate::notify;
use crate::stargazer_list::DiscoveredStargazer;
use crate::stargazer_page::{self, append_chat_row};
use crate::util::{approximate_aspects, sign_glyph};

// ── init ──────────────────────────────────────────────────────────────────────

pub struct AppInit {
    pub config: LocalConfig,
    pub store: ZodiaStore,
    pub baseline: BaselineStore,
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
    /// User tapped a stargazer row — connect (if needed) then open their page.
    OpenStargazer(PeerId),
    CallStargazer(PeerId),
    /// User approved an incoming consent request.
    AcceptConsent,
    /// User declined an incoming consent request.
    RejectConsent,
    AcceptCall,
    RejectCall,
    HangUp,
    /// User sent a chat message to a connected peer.
    SendChat { peer_id: PeerId, text: String },
    /// Send a message to `dest` via `relay` (blind relay path).
    ///
    /// The relay peer will forward the ECIES-encrypted payload to `dest` without
    /// being able to read it.  Both `relay` and `dest` must be connected peers.
    SendViaRelay { relay: PeerId, dest: PeerId, text: String },
    /// User set or updated a nickname for a connected peer.
    SetNickname { peer_id: PeerId, name: String },
    /// "+" pressed in the Stargazers view — stage outgoing consent proposal, no navigation yet.
    ProposeConsent(PeerId),
    /// "Share ✓" in outgoing consent bar — proceed with the actual connection.
    ConfirmOutgoingConsent,
    /// "Cancel ✕" in outgoing consent bar — discard the proposal.
    CancelOutgoingConsent,
    /// Internal: background connect+consent completed; finalize on component thread.
    ConnectionComplete {
        peer_id: PeerId,
        their_blob: ConsentBlob,
        channel: DirectChannel,
        /// If true, push the stargazer page after updating state.
        navigate: bool,
        /// If true, run interp sync and persist to peers.tsv (new connection).
        /// If false, skip both (reconnect — peer already persisted).
        is_new: bool,
    },
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
    baseline: Rc<BaselineStore>,

    network: Option<Arc<ZodiaNetwork>>,
    node_id_text: String,

    /// Stargazers seen on the gossip swarm (Tier-0), ordered by discovery time.
    discovered_stargazers: Vec<DiscoveredStargazer>,
    /// Stargazers whose Tier-1 exchange has completed.
    connected_stargazers: HashMap<PeerId, ConsentBlob>,
    /// Active QUIC channels — presence means the channel is open.
    connected_channels: HashMap<PeerId, DirectChannel>,
    /// Explicit presence state received from each stargazer over their channel.
    stargazer_status: HashMap<PeerId, PeerStatus>,

    /// Incremented whenever the stargazer list content changes so `update_view`
    /// knows when to rebuild the GTK rows.
    stargazer_list_generation: u64,

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

    /// User-assigned nicknames, keyed by 4-byte upper-hex stargazer tag.
    stargazer_nicknames: HashMap<String, String>,
    /// Unread message counts per peer (cleared when their page is opened).
    unread_messages: HashMap<String, usize>,

    /// Incoming consent requests waiting for user approval, in arrival order.
    /// The first entry is the one currently shown in the consent bar.
    pending_consents: VecDeque<(PeerId, DirectChannel)>,

    /// Outgoing consent proposal staged by "+" before any network I/O.
    pending_outgoing_consent: Option<PeerId>,

    /// Channel to the background LogSync task for publishing new interpretations.
    /// `None` until the network is up.
    sync_publish_tx: Option<tokio::sync::mpsc::Sender<SyncPublishMsg>>,

    /// Most recent community interpretation contributions, for the network tab.
    recent_interps: Vec<zodia_store::RecentInterp>,
}

// ── widgets ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct AppWidgets {
    outer_stack: gtk::Stack,
    setup_status: gtk::Label,

    chart_container: gtk::Box,
    sky_container: gtk::Box,

    /// Overlay split view — sidebar on the left, content stack on the right.
    split_view: adw::OverlaySplitView,
    /// Single nav ListBox (Chart / Sky / Stargazers + opened pages) — one selection source.
    nav_list: gtk::ListBox,
    /// Generation of the stargazer list we last rendered.
    stargazer_list_shown_gen: u64,

    /// Single content stack — chart / sky / stargazers + per-stargazer pages, all as named children.
    content_stack: gtk::Stack,
    /// The "Stargazers" scrollable view (rebuilt for discovered/online stargazers).
    stargazers_content: gtk::Box,

    /// Message list widget per stargazer (keyed by 4-byte hex tag).
    stargazer_msg_lists: HashMap<String, gtk::ListBox>,
    /// How many messages from `chat_logs` have already been appended to each list.
    stargazer_chat_shown: HashMap<String, usize>,
    /// Call and send buttons per stargazer — disabled when stargazer is offline.
    stargazer_actions: HashMap<String, (gtk::Button, gtk::Button, gtk::Entry)>,
    /// ViewSwitcherTitle per stargazer — updated when the nickname changes.
    #[allow(deprecated)]
    stargazer_titles: HashMap<String, adw::ViewSwitcherTitle>,

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

    consent_bar: gtk::Box,
    consent_status: gtk::Label,
    consent_accept_btn: gtk::Button,
    consent_reject_btn: gtk::Button,

    outgoing_consent_bar: gtk::Box,
    outgoing_consent_status: gtk::Label,
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
        let baseline = Rc::new(init.baseline);

        let stargazer_nicknames = load_nicknames(init.config.data_dir());
        let persisted_stargazers = load_stargazers(init.config.data_dir());

        // Pre-load chat history for all persisted stargazers.
        let chat_logs: HashMap<PeerId, Vec<(bool, String)>> = persisted_stargazers
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
            baseline,
            network: None,
            node_id_text: String::new(),
            discovered_stargazers: Vec::new(),
            connected_stargazers: persisted_stargazers,
            connected_channels: HashMap::new(),
            stargazer_status: HashMap::new(),
            stargazer_list_generation: 0,
            pending_push_queue: RefCell::new(Vec::new()),
            config: init.config,
            setup_error: String::new(),
            identity,
            call_state: CallState::Idle,
            active_audio: None,
            chat_logs,
            stargazer_nicknames,
            unread_messages: HashMap::new(),
            pending_consents: VecDeque::new(),
            pending_outgoing_consent: None,
            sync_publish_tx: None,
            recent_interps: Vec::new(),
        };
        model.recent_interps = model.store.borrow()
            .recent_community_interps(12).unwrap_or_default();

        if let Some(birth) = model.config.birth.clone() {
            if let Ok(chart) = Chart::compute(birth.clone()) {
                model.chart = Some(chart);
            }
        }

        let widgets = build_widgets(&root, &model, &sender);

        // Register GIO actions used by interactive notification buttons.
        notify::register_actions(&sender);

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
                model.network = Some(Arc::new(net));
                start_network_command(&sender, rx);
                sender.input(AppMsg::NetworkReady);
                // Kick off periodic re-announce loop starting in 20 s.
                let s2 = sender.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
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
                    self.network = Some(Arc::new(net));
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
                let peer_ids: Vec<PeerId> = self.connected_stargazers.keys().cloned().collect();
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
                // Schedule the next announce in 20 s.
                let s = sender.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
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
                if !self.connected_stargazers.contains_key(&peer_id)
                    || self.connected_channels.contains_key(&peer_id)
                {
                    return;
                }
                if let (Some(net), Some(our_blob)) = (
                    &self.network,
                    make_consent_blob(&self.config, &self.identity),
                ) {
                    let net = Arc::clone(net);
                    let pid = peer_id.clone();
                    let peer_hex = hex::encode_upper(&pid.0[..4]);
                    let s = sender.clone();
                    info!(peer = %peer_hex, "attempting auto-reconnect");
                    tokio::spawn(async move {
                        match net.connect_peer(&pid).await {
                            Ok(channel) => {
                                match channel.exchange_consent(&our_blob).await {
                                    Ok(their_blob) => {
                                        info!(peer = %peer_hex, "auto-reconnect consent exchange ok");
                                        s.input(AppMsg::ConnectionComplete {
                                            peer_id: pid,
                                            their_blob,
                                            channel,
                                            navigate: false,
                                            is_new: false,
                                        });
                                    }
                                    Err(e) => warn!(peer = %peer_hex, "auto-reconnect consent exchange: {e}"),
                                }
                            }
                            Err(e) => warn!(peer = %peer_hex, "auto-reconnect failed: {e}"),
                        }
                    });
                }
            }

            AppMsg::SetNickname { peer_id, name } => {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                if name.trim().is_empty() {
                    self.stargazer_nicknames.remove(&tag);
                } else {
                    self.stargazer_nicknames.insert(tag, name.trim().to_string());
                }
                save_nicknames(self.config.data_dir(), &self.stargazer_nicknames);
                self.stargazer_list_generation += 1;
            }

                        AppMsg::ProposeConsent(peer_id) => {
                // "+" from Stargazers view — stage proposal before any network I/O.
                self.pending_outgoing_consent = Some(peer_id);
                self.stargazer_list_generation += 1;
            }

            AppMsg::ConfirmOutgoingConsent => {
                // "Share ✓" — spawn background connect+consent so we don't block
                // the component thread (and stall mDNS / PeerDiscovered events).
                if let Some(peer_id) = self.pending_outgoing_consent.take() {
                    if !self.connected_stargazers.contains_key(&peer_id) {
                        if let (Some(net), Some(our_blob)) = (
                            &self.network,
                            make_consent_blob(&self.config, &self.identity),
                        ) {
                            let net = Arc::clone(net);
                            let pid = peer_id.clone();
                            let s = sender.clone();
                            tokio::spawn(async move {
                                let peer_hex = hex::encode_upper(&pid.0[..4]);
                                match net.connect_peer(&pid).await {
                                    Ok(channel) => {
                                        info!(peer = %peer_hex, "consent channel opened");
                                        match channel.exchange_consent(&our_blob).await {
                                            Ok(their_blob) => {
                                                info!(peer = %peer_hex, "consent exchange complete");
                                                s.input(AppMsg::ConnectionComplete {
                                                    peer_id: pid,
                                                    their_blob,
                                                    channel,
                                                    navigate: true,
                                                    is_new: true,
                                                });
                                            }
                                            Err(e) => warn!("consent exchange: {e}"),
                                        }
                                    }
                                    Err(e) => error!("connect_peer: {e}"),
                                }
                            });
                        }
                    }
                }
                self.stargazer_list_generation += 1; // hides bar regardless of outcome
            }

            AppMsg::CancelOutgoingConsent => {
                self.pending_outgoing_consent = None;
                self.stargazer_list_generation += 1;
            }

            AppMsg::ConnectionComplete { peer_id, their_blob, channel, navigate, is_new } => {
                let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                if is_new {
                    let name = self.stargazer_nicknames.get(&peer_hex)
                        .cloned()
                        .unwrap_or_else(|| format!("···{peer_hex}"));
                    notify::send(
                        &format!("connected-{peer_hex}"),
                        "Connected",
                        &format!("Now exchanging charts with {name}"),
                        "network-wireless-symbolic",
                        &[],
                    );
                    // Run interp sync on the component thread (uses Rc types).
                    do_interp_sync(
                        &channel, &their_blob,
                        self.chart.as_ref(), &self.store,
                        &self.identity, &peer_hex,
                    ).await;
                    self.connected_stargazers.insert(peer_id.clone(), their_blob);
                    save_stargazers(self.config.data_dir(), &self.connected_stargazers);
                } else {
                    // Reconnect — update stored blob in case keys rotated.
                    self.connected_stargazers.insert(peer_id.clone(), their_blob);
                }
                if let Some(net) = &self.network {
                    net.accept_channel(peer_id.clone(), channel.clone());
                }
                send_status_active(&channel);
                self.connected_channels.insert(peer_id.clone(), channel);
                self.stargazer_list_generation += 1;
                if navigate {
                    self.pending_push_queue.borrow_mut().push(peer_id);
                }
            }

            AppMsg::OpenStargazer(peer_id) => {
                // Sidebar tap — navigate to stargazer page; connect first if needed.
                let tag = hex::encode_upper(&peer_id.0[..4]);
                self.unread_messages.remove(&tag);

                if self.connected_stargazers.contains_key(&peer_id) {
                    // Already know this peer — navigate immediately.
                    self.pending_push_queue.borrow_mut().push(peer_id);
                } else {
                    // Not connected yet — spawn background connect+consent; navigation
                    // is queued once ConnectionComplete fires on the component thread.
                    if let (Some(net), Some(our_blob)) = (
                        &self.network,
                        make_consent_blob(&self.config, &self.identity),
                    ) {
                        let net = Arc::clone(net);
                        let pid = peer_id.clone();
                        let s = sender.clone();
                        tokio::spawn(async move {
                            let peer_hex = hex::encode_upper(&pid.0[..4]);
                            match net.connect_peer(&pid).await {
                                Ok(channel) => {
                                    info!(peer = %peer_hex, "consent channel opened");
                                    match channel.exchange_consent(&our_blob).await {
                                        Ok(their_blob) => {
                                            info!(peer = %peer_hex, "consent exchange complete");
                                            s.input(AppMsg::ConnectionComplete {
                                                peer_id: pid,
                                                their_blob,
                                                channel,
                                                navigate: true,
                                                is_new: true,
                                            });
                                        }
                                        Err(e) => warn!("consent exchange: {e}"),
                                    }
                                }
                                Err(e) => error!("connect_peer: {e}"),
                            }
                        });
                    }
                }
            }

            AppMsg::CallStargazer(peer_id) => {
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

            AppMsg::AcceptConsent => {
                if let Some((peer_id, channel)) = self.pending_consents.pop_front() {
                    let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                    notify::withdraw(&format!("consent-{peer_hex}"));
                    if let Some(net) = &self.network {
                        if let Some(our_blob) = make_consent_blob(&self.config, &self.identity) {
                            match channel.exchange_consent(&our_blob).await {
                                Ok(their_blob) => {
                                    info!(peer = %peer_hex, "consent exchange complete");
                                    do_interp_sync(
                                        &channel, &their_blob,
                                        self.chart.as_ref(), &self.store,
                                        &self.identity, &peer_hex,
                                    ).await;
                                    self.connected_stargazers.insert(peer_id.clone(), their_blob);
                                    save_stargazers(self.config.data_dir(), &self.connected_stargazers);
                                    self.stargazer_list_generation += 1;
                                }
                                Err(e) => warn!(peer = %peer_hex, "consent exchange failed: {e}"),
                            }
                        }
                        net.accept_channel(peer_id.clone(), channel.clone());
                        send_status_active(&channel);
                        self.connected_channels.insert(peer_id, channel);
                    }
                    self.stargazer_list_generation += 1;
                }
            }

            AppMsg::RejectConsent => {
                if let Some((peer_id, _channel)) = self.pending_consents.pop_front() {
                    // Dropping _channel closes the QUIC connection.
                    let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                    notify::withdraw(&format!("consent-{peer_hex}"));
                    info!(peer = %peer_hex, "consent request declined");
                    self.stargazer_list_generation += 1;
                }
            }

            AppMsg::AcceptCall => {
                if let CallState::Ringing { peer_id, session_id } = &self.call_state {
                    let peer_id = peer_id.clone();
                    let session_id = *session_id;
                    notify::withdraw(&format!("call-{}", hex::encode_upper(&peer_id.0[..4])));
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
                    notify::withdraw(&format!("call-{}", hex::encode_upper(&peer_id.0[..4])));
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

            AppMsg::SendViaRelay { relay, dest, text } => {
                let Some(relay_channel) = self.connected_channels.get(&relay) else { return };
                let Some(their_blob) = self.connected_stargazers.get(&dest) else { return };
                let our_id = self.network.as_ref().map(|n| n.node_id()).unwrap_or(PeerId([0u8; 32]));

                let inner = RelayPayload { from: our_id.0, text: text.clone() };
                let mut cbor = Vec::new();
                if ciborium::into_writer(&inner, &mut cbor).is_err() { return };
                let payload = ecies_encrypt(&their_blob.relay_pk, &cbor);
                let msg = ChannelMsg::RelayMsg { dest: dest.0, payload };
                if relay_channel.send_msg(&msg).await.is_ok() {
                    let _ = self.store.borrow().insert_message(&dest.0, true, &text);
                    self.chat_logs.entry(dest).or_default().push((true, text));
                }
            }
            AppMsg::ShareInterp(entry) => {
                // Reload activity feed (insert_signed already ran in aspect_view).
                self.recent_interps = self.store.borrow()
                    .recent_community_interps(12).unwrap_or_default();
                self.stargazer_list_generation += 1;
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
                // Reload activity feed and trigger a network view refresh.
                self.recent_interps = self.store.borrow()
                    .recent_community_interps(12).unwrap_or_default();
                self.stargazer_list_generation += 1;
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
                self.discovered_stargazers.push(DiscoveredStargazer::from_blob(peer_id, &blob, approx));
                self.stargazer_list_generation += 1;
                // This peer just reached us via gossip, meaning our overlay now includes
                // them.  Re-publish our own announce immediately so they can discover
                // us too — without this, mutual discovery relies solely on the periodic
                // re-announce timer (up to 20 s latency after the overlay connects).
                if let Some(net) = &self.network {
                    if let Err(e) = net.publish_announce().await {
                        warn!("re-announce on peer-discovered failed: {e}");
                    }
                }
            }
            ZodiaNetEvent::PeerLeft { peer_id } => {
                self.discovered_stargazers.retain(|p| p.peer_id != peer_id);
                self.stargazer_list_generation += 1;
            }
            ZodiaNetEvent::IncomingChannel { peer_id, channel } => {
                let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                info!(peer = %peer_hex, "incoming consent request — waiting for user approval");
                let name = self.stargazer_nicknames.get(&peer_hex)
                    .cloned()
                    .unwrap_or_else(|| format!("···{peer_hex}"));
                notify::send(
                    &format!("consent-{peer_hex}"),
                    "Chart exchange request",
                    &format!("{name} wants to exchange charts"),
                    "mail-unread-symbolic",
                    &[("Accept", "app.accept-consent"), ("Decline", "app.reject-consent")],
                );
                self.pending_consents.push_back((peer_id, channel));
                self.stargazer_list_generation += 1; // triggers update_view → consent bar refresh
            }
            ZodiaNetEvent::CallOffer { from, session_id } => {
                let peer_hex = hex::encode_upper(&from.0[..4]);
                let name = self.stargazer_nicknames.get(&peer_hex)
                    .cloned()
                    .unwrap_or_else(|| format!("···{peer_hex}"));
                notify::send(
                    &format!("call-{peer_hex}"),
                    "Incoming call",
                    &format!("{name} is calling"),
                    "call-start-symbolic",
                    &[("Accept", "app.accept-call"), ("Decline", "app.reject-call")],
                );
                self.call_state = CallState::Ringing { peer_id: from, session_id };
            }
            ZodiaNetEvent::CallAccepted { from, .. } => {
                self.call_state = CallState::Active { peer_id: from };
            }
            ZodiaNetEvent::CallRejected { .. } => {
                if let Some(pid) = self.call_state.active_peer() {
                    notify::withdraw(&format!("call-{}", hex::encode_upper(&pid.0[..4])));
                }
                self.active_audio = None;
                self.call_state = CallState::Idle;
            }
            ZodiaNetEvent::CallHungUp { .. } => {
                if let Some(pid) = self.call_state.active_peer() {
                    notify::withdraw(&format!("call-{}", hex::encode_upper(&pid.0[..4])));
                }
                self.active_audio = None;
                self.call_state = CallState::Idle;
            }
            ZodiaNetEvent::ChatReceived { from, text } => {
                let tag = hex::encode_upper(&from.0[..4]);
                let name = self.stargazer_nicknames.get(&tag)
                    .cloned()
                    .unwrap_or_else(|| format!("···{tag}"));
                let preview: String = text.chars().take(80).collect();
                notify::send(
                    &format!("chat-{tag}"),
                    &name,
                    &preview,
                    "chat-message-new-symbolic",
                    &[],
                );
                *self.unread_messages.entry(tag).or_insert(0) += 1;
                let _ = self.store.borrow().insert_message(&from.0, false, &text);
                self.chat_logs.entry(from).or_default().push((false, text));
            }
            ZodiaNetEvent::PeerStatusChanged { peer_id, status } => {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                info!(peer = %tag, ?status, "peer status update");
                self.stargazer_status.insert(peer_id, status);
                self.stargazer_list_generation += 1;
            }
            ZodiaNetEvent::RelayReceived { via: _, dest, payload } => {
                let our_id = self.network.as_ref().map(|n| n.node_id()).unwrap_or(PeerId([0u8; 32]));
                if dest == our_id {
                    // We are the final destination — decrypt and deliver as chat.
                    let sk = self.identity.relay_secret_bytes();
                    match ecies_decrypt(&sk, &payload) {
                        Ok(plaintext) => {
                            match ciborium::from_reader::<RelayPayload, _>(plaintext.as_slice()) {
                                Ok(rp) => {
                                    let from = PeerId(rp.from);
                                    let tag = hex::encode_upper(&from.0[..4]);
                                    *self.unread_messages.entry(tag).or_insert(0) += 1;
                                    let _ = self.store.borrow().insert_message(&from.0, false, &rp.text);
                                    self.chat_logs.entry(from).or_default().push((false, rp.text));
                                }
                                Err(e) => warn!("relay: CBOR decode failed: {e}"),
                            }
                        }
                        Err(e) => warn!("relay: ECIES decrypt failed: {e}"),
                    }
                } else if let Some(fwd_channel) = self.connected_channels.get(&dest) {
                    // We are a relay node — forward the opaque payload without decrypting.
                    let msg = ChannelMsg::RelayMsg { dest: dest.0, payload };
                    let ch = fwd_channel.clone();
                    tokio::spawn(async move {
                        if let Err(e) = ch.send_msg(&msg).await {
                            debug!("relay forward failed: {e}");
                        }
                    });
                } else {
                    debug!(
                        dest = %hex::encode_upper(&dest.0[..4]),
                        "relay: no channel to destination, dropping"
                    );
                }
            }

            ZodiaNetEvent::PeerChannelClosed { peer_id } => {
                self.stargazer_status.remove(&peer_id);
                self.connected_channels.remove(&peer_id);
                self.stargazer_list_generation += 1;
                // If we have a Tier-1 relationship with this peer, schedule a
                // reconnect attempt after 10 s to restore the channel.
                if self.connected_stargazers.contains_key(&peer_id) {
                    let s = _sender.clone();
                    let pid = peer_id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        s.input(AppMsg::Reconnect(pid));
                    });
                }
            }
            ZodiaNetEvent::InterpReceived { from, entries } => {
                let peer_hex = hex::encode_upper(&from.0[..4]);
                let n = import_interps(&entries, &self.store, &peer_hex);
                if n > 0 {
                    info!(peer = %peer_hex, "imported {n} live interpretations from peer");
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
                    Rc::clone(&self.baseline),
                    Rc::clone(&self.identity),
                    sender.clone(),
                );
                nav.widget().set_vexpand(true);
                widgets.chart_container.append(nav.widget());

                if let Ok(ts) = chart.transits_at(current_jdn()) {
                    let tav = AspectView::transits(
                        aspect_list::transit_items(
                            &ts.transit_aspects,
                            &ts.house_transits,
                            &chart.positions,
                            ts.transit_jdn,
                        ),
                        Rc::clone(&self.store),
                        Rc::clone(&self.baseline),
                        Rc::clone(&self.identity),
                        sender.clone(),
                    );
                    tav.widget().set_vexpand(true);
                    widgets.sky_container.append(tav.widget());
                }
            }
        }

        // ── rebuild peer list when content changes ────────────────────────────

        if self.stargazer_list_generation != widgets.stargazer_list_shown_gen {
            rebuild_sidebar_stargazers(widgets, self, &sender);
            rebuild_network_view(widgets, self, &sender);
            widgets.stargazer_list_shown_gen = self.stargazer_list_generation;
        }

        // ── push peer pages for OpenPeer requests ─────────────────────────────

        let pending: Vec<PeerId> = self.pending_push_queue.borrow_mut().drain(..).collect();
        for peer_id in pending {
            if let Some(their_blob) = self.connected_stargazers.get(&peer_id) {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                if let Some(chart) = &self.chart {
                    let nickname = self.stargazer_nicknames.get(&tag).map(|s| s.as_str());
                    if widgets.content_stack.child_by_name(&tag).is_some() {
                        // Page already built — switch to it directly.
                        widgets.content_stack.set_visible_child_name(&tag);
                    } else {
                        let (toolbar_view, msg_list, call_btn, send_btn, entry, switcher_title) =
                            stargazer_page::build_stargazer_page(
                                &peer_id, their_blob, chart,
                                Rc::clone(&self.store),
                                Rc::clone(&self.baseline),
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
                        widgets.stargazer_msg_lists.insert(tag.clone(), msg_list);
                        widgets.stargazer_actions.insert(tag.clone(), (call_btn, send_btn, entry));
                        widgets.stargazer_titles.insert(tag, switcher_title);
                    }
                    // On narrow windows, hide the sidebar so the peer page
                    // has full width.  The ToggleButton in the peer header
                    // brings it back.
                    let w = widgets.split_view.width();
                    if w > 0 && w < 720 {
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
            let shown = widgets.stargazer_chat_shown.get(&tag).copied().unwrap_or(0);
            if messages.len() > shown {
                if let Some(list) = widgets.stargazer_msg_lists.get(&tag) {
                    for (from_us, text) in &messages[shown..] {
                        append_chat_row(list, text, *from_us);
                    }
                    widgets.stargazer_chat_shown.insert(tag, messages.len());
                }
            }
        }

        // ── update call/send button sensitivity for open peer pages ──────────

        for (tag, (call_btn, send_btn, entry)) in &widgets.stargazer_actions {
            let online = self.connected_channels.keys()
                .any(|id| hex::encode_upper(&id.0[..4]) == *tag);
            call_btn.set_sensitive(online);
            send_btn.set_sensitive(online);
            entry.set_sensitive(online);
        }

        // ── network status label (shown in the Network content view) ─────────

        {
            let connected = self.connected_stargazers.len();
            let active    = self.stargazer_status.values()
                .filter(|s| **s == PeerStatus::Active).count();
            let text = if self.node_id_text.is_empty() {
                "Starting up…".to_string()
            } else if connected == 0 {
                format!("Node ···{}  ·  searching…", self.node_id_text)
            } else {
                format!("Node ···{}  ·  {} connected  ·  {} online",
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
                        let name = self.stargazer_nicknames.get(tag)
                            .cloned()
                            .unwrap_or_else(|| format!("···{tag}"));
                        format!("{name}  ·  {n} unread")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                widgets.notif_label.set_text(&lines);
            }
        }

        // ── consent bar ──────────────────────────────────────────────────────

        if let Some((peer_id, _)) = self.pending_consents.front() {
            let tag = hex::encode_upper(&peer_id.0[..4]);
            // Show solar glyph if we've seen their announce blob.
            let glyph = self.discovered_stargazers.iter()
                .find(|p| &p.peer_id == peer_id)
                .map(|p| sign_glyph(p.solar_month).to_string())
                .unwrap_or_default();
            let more = self.pending_consents.len().saturating_sub(1);
            let label = if more == 0 {
                format!("{glyph}  ···{tag} wants to connect")
            } else {
                format!("{glyph}  ···{tag} wants to connect  (+{more} more)")
            };
            widgets.consent_status.set_text(&label);
            widgets.consent_bar.set_visible(true);
        } else {
            widgets.consent_bar.set_visible(false);
        }

        // ── outgoing consent bar ─────────────────────────────────────────────

        if let Some(peer_id) = &self.pending_outgoing_consent {
            let tag = hex::encode_upper(&peer_id.0[..4]);
            let glyph = self.discovered_stargazers.iter()
                .find(|p| &p.peer_id == peer_id)
                .map(|p| sign_glyph(p.solar_month).to_string())
                .unwrap_or_default();
            widgets.outgoing_consent_status.set_text(
                &format!("{glyph}  Share your chart with ···{tag}?")
            );
            widgets.outgoing_consent_bar.set_visible(true);
        } else {
            widgets.outgoing_consent_bar.set_visible(false);
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
fn rebuild_sidebar_stargazers(
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

    let mut sorted: Vec<&PeerId> = model.connected_stargazers.keys().collect();
    sorted.sort_by_key(|id| hex::encode_upper(&id.0[..4]));

    // Show the "Connected" section header only when there are connected peers.
    if let Some(header) = widgets.nav_list.row_at_index(3) {
        header.set_visible(!sorted.is_empty());
    }

    for peer_id in sorted {
        let their_blob   = &model.connected_stargazers[peer_id];
        let peer_hex     = hex::encode_upper(&peer_id.0[..4]);
        let solar_month  = zodia_core::solar_month(their_blob.birth.jdn);
        let glyph        = sign_glyph(solar_month);
        let status       = model.stargazer_status.get(peer_id);
        let has_channel  = model.connected_channels.contains_key(peer_id);
        let display_name = model.stargazer_nicknames.get(&peer_hex)
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
            let current = model.stargazer_nicknames.get(&peer_hex).cloned().unwrap_or_default();
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
        if let Some(title_widget) = widgets.stargazer_titles.get(&peer_hex) {
            let title_text = model.stargazer_nicknames.get(&peer_hex)
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
/// Format a canonical interp key into a readable `(kind, description)` pair.
/// "natal:jupiter_trine_venus" → ("Natal", "Jupiter trine Venus")
fn format_interp_key(key: &str) -> (String, String) {
    let (kind, rest) = key.split_once(':').unwrap_or(("", key));
    let kind_label = match kind {
        "natal"         => "Natal",
        "synastry"      => "Synastry",
        "transit"       => "Transit",
        "house_transit" => "House transit",
        other           => other,
    };
    let desc = rest.replace('_', " ");
    // Capitalise first character only.
    let mut chars = desc.chars();
    let desc = match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None    => desc,
    };
    (kind_label.to_string(), desc)
}

fn rebuild_network_view(
    widgets: &mut AppWidgets,
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) {
    // Remove everything except the first child (net_status_label).
    loop {
        match widgets.stargazers_content.last_child() {
            Some(child) if child != widgets.stargazers_content.first_child().unwrap() => {
                widgets.stargazers_content.remove(&child);
            }
            _ => break,
        }
    }

    let discoverable: Vec<&DiscoveredStargazer> = model.discovered_stargazers.iter()
        .filter(|p| !model.connected_stargazers.contains_key(&p.peer_id))
        .collect();

    if discoverable.is_empty() {
        let status = adw::StatusPage::new();
        status.set_icon_name(Some("network-wireless-symbolic"));
        status.set_title("No current online Zodia users found");
        status.set_description(Some(
            "Other Zodia users will appear here as they are discovered.",
        ));
        widgets.stargazers_content.append(&status);
    } else {
        let group = adw::PreferencesGroup::new();
        let n = discoverable.len();
        group.set_title(&format!(
            "{n} user{} on the network",
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
            add_btn.connect_clicked(move |_| s.input(AppMsg::ProposeConsent(pid.clone())));
            row.add_suffix(&add_btn);

            group.add(&row);
        }
        widgets.stargazers_content.append(&group);
    }

    // ── Recent contributions ──────────────────────────────────────────────────
    if !model.recent_interps.is_empty() {
        let contrib_group = adw::PreferencesGroup::new();
        contrib_group.set_title("Recent Contributions");
        contrib_group.set_description(Some("Interpretations shared across the network"));

        for interp in &model.recent_interps {
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

            // Kind badge (Natal / Synastry / Transit / House transit) as prefix.
            let kind_lbl = gtk::Label::new(Some(&kind));
            kind_lbl.add_css_class("caption");
            kind_lbl.add_css_class("dim-label");
            kind_lbl.set_valign(gtk::Align::Center);
            row.add_prefix(&kind_lbl);

            contrib_group.add(&row);
        }
        widgets.stargazers_content.append(&contrib_group);
    }
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

fn make_consent_blob(config: &LocalConfig, identity: &IdentityKeypair) -> Option<ConsentBlob> {
    config.birth.as_ref().map(|birth| ConsentBlob {
        birth: birth.clone(),
        prekey:    [0u8; 32],
        ephemeral: [0u8; 32],
        relay_pk:  identity.relay_public_key(),
    })
}

// ── interpretation sync ───────────────────────────────────────────────────────

async fn do_interp_sync(
    channel: &DirectChannel,
    their_blob: &ConsentBlob,
    our_chart: Option<&Chart>,
    store: &Rc<RefCell<ZodiaStore>>,
    identity: &Rc<IdentityKeypair>,
    peer_hex: &str,
) {
    let outgoing = collect_entries_for_stargazer(their_blob, our_chart, store, identity);
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

fn collect_entries_for_stargazer(
    their_blob: &ConsentBlob,
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
        content_stack, stargazers_content,
        notif_btn, notif_label,
        net_status_label,
        consent_bar, consent_status, consent_accept_btn, consent_reject_btn,
        outgoing_consent_bar, outgoing_consent_status,
        call_bar, call_status, accept_btn, hangup_btn,
    ) = build_main_page(model, sender);
    outer_stack.add_named(&main_view, Some("main"));

    // ── Responsive sidebar collapse via adw::Breakpoint ──────────────────────
    // Below 720 px: collapse the sidebar (burger button appears in headers).
    // Above 720 px: sidebar is always visible side-by-side.
    // adw::OverlaySplitView doesn't auto-collapse; we drive `collapsed` through
    // a breakpoint so it integrates with the ADW adaptive layout system.
    {
        let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Px,
        ));
        bp.add_setter(&split_view, "collapsed", Some(&true.to_value()));
        let sv = split_view.clone();
        bp.connect_unapply(move |_| {
            // Returning to wide layout: make the sidebar visible again.
            sv.set_show_sidebar(true);
        });
        root.add_breakpoint(bp);
    }

    // Populate aspect views for returning users with an existing chart.
    if let Some(chart) = &model.chart {
        let nav = AspectView::natal(
            aspect_list::natal_items(&chart.natal_aspects()),
            chart,
            Rc::clone(&model.store),
            Rc::clone(&model.baseline),
            Rc::clone(&model.identity),
            sender.clone(),
        );
        nav.widget().set_vexpand(true);
        chart_container.append(nav.widget());

        if let Ok(ts) = chart.transits_at(current_jdn()) {
            let tav = AspectView::transits(
                aspect_list::transit_items(
                    &ts.transit_aspects,
                    &ts.house_transits,
                    &chart.positions,
                    ts.transit_jdn,
                ),
                Rc::clone(&model.store),
                Rc::clone(&model.baseline),
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
        stargazer_list_shown_gen: u64::MAX, // force initial build
        content_stack,
        stargazers_content,
        stargazer_msg_lists: HashMap::new(),
        stargazer_chat_shown: HashMap::new(),
        stargazer_actions: HashMap::new(),
        stargazer_titles: HashMap::new(),
        notif_btn,
        notif_label,
        net_status_label,
        consent_bar,
        consent_status,
        consent_accept_btn,
        consent_reject_btn,
        outgoing_consent_bar,
        outgoing_consent_status,
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
    date_group.set_title("Birth Date &amp; Time");
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

    let selected_loc: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));

    // ── City search section ───────────────────────────────────────────────────
    // adw::EntryRow for input; results appear as inline ActionRows in the group.
    let city_section = gtk::Box::new(gtk::Orientation::Vertical, 12);

    let city_group = adw::PreferencesGroup::new();
    city_group.set_title("Birth Location");

    let city_row = adw::EntryRow::new();
    city_row.set_title("City");
    city_row.set_input_purpose(gtk::InputPurpose::Name);
    city_row.set_input_hints(gtk::InputHints::NO_SPELLCHECK | gtk::InputHints::NO_EMOJI);
    city_group.add(&city_row);

    let coord_row = adw::ActionRow::new();
    coord_row.set_title("Coordinates");
    coord_row.set_subtitle("—");
    coord_row.set_selectable(false);
    coord_row.set_sensitive(false);
    city_group.add(&coord_row);

    city_section.append(&city_group);

    let to_manual_btn = gtk::Button::with_label("Enter coordinates manually");
    to_manual_btn.add_css_class("flat");
    city_section.append(&to_manual_btn);

    {
        let result_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
        let selecting: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let loc     = selected_loc.clone();
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

                    let row = adw::ActionRow::new();
                    row.set_title(&label);
                    row.set_activatable(true);

                    let loc2     = loc.clone();
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

    // ── Manual lat/lon section ────────────────────────────────────────────────
    let manual_section = gtk::Box::new(gtk::Orientation::Vertical, 12);

    let manual_group = adw::PreferencesGroup::new();
    manual_group.set_title("Birth Location");

    let lat_row = adw::SpinRow::with_range(-90.0, 90.0, 0.0001);
    lat_row.set_title("Latitude");
    lat_row.set_digits(4);
    manual_group.add(&lat_row);

    let lon_row = adw::SpinRow::with_range(-180.0, 180.0, 0.0001);
    lon_row.set_title("Longitude");
    lon_row.set_digits(4);
    manual_group.add(&lon_row);

    manual_section.append(&manual_group);

    // Keep selected_loc in sync while manual mode is active (only updates when Some).
    {
        let loc = selected_loc.clone();
        lat_row.connect_value_notify(move |row| {
            if let Some(ref mut v) = *loc.borrow_mut() { v.0 = row.value(); }
        });
    }
    {
        let loc = selected_loc.clone();
        lon_row.connect_value_notify(move |row| {
            if let Some(ref mut v) = *loc.borrow_mut() { v.1 = row.value(); }
        });
    }

    // ── Visibility + toggle wiring ────────────────────────────────────────────
    if zodia_core::has_cities() {
        // Default: city search visible, manual hidden.
        manual_section.set_visible(false);

        {
            let cs   = city_section.clone();
            let ms   = manual_section.clone();
            let loc  = selected_loc.clone();
            let latr = lat_row.clone();
            let lonr = lon_row.clone();
            to_manual_btn.connect_clicked(move |_| {
                cs.set_visible(false);
                ms.set_visible(true);
                *loc.borrow_mut() = Some((latr.value(), lonr.value()));
            });
        }

        let back_btn = gtk::Button::with_label("Search by city name");
        back_btn.add_css_class("flat");
        manual_section.append(&back_btn);

        {
            let cs  = city_section.clone();
            let ms  = manual_section.clone();
            let loc = selected_loc.clone();
            back_btn.connect_clicked(move |_| {
                ms.set_visible(false);
                cs.set_visible(true);
                *loc.borrow_mut() = None;
            });
        }
    } else {
        // No city data compiled in: skip straight to manual, no toggle back.
        city_section.set_visible(false);
        *selected_loc.borrow_mut() = Some((0.0, 0.0));
    }

    content.append(&city_section);
    content.append(&manual_section);

    let setup_status = gtk::Label::new(None);
    setup_status.add_css_class("error");
    content.append(&setup_status);

    let btn = gtk::Button::with_label("Begin  →");
    btn.add_css_class("suggested-action");
    btn.add_css_class("pill");
    btn.set_halign(gtk::Align::Center);
    content.append(&btn);

    let s = sender.clone();
    let (yr, mr, dr, hr, minr) = (
        year_row.clone(), month_row.clone(), day_row.clone(),
        hour_row.clone(), minute_row.clone(),
    );
    let loc = selected_loc.clone();
    btn.connect_clicked(move |_| {
        let (lat, lon) = match *loc.borrow() {
            Some(v) => v,
            None => { s.input(AppMsg::SetupError("Select a birth location".into())); return; }
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
    adw::ToolbarView,                                   // outermost wrapper
    gtk::Box, gtk::Box,                                 // chart_container, sky_container
    adw::OverlaySplitView, gtk::ListBox,                 // split_view, nav_list
    gtk::Stack, gtk::Box,                               // content_stack, stargazers_content
    gtk::MenuButton, gtk::Label,                        // notif_btn, notif_label
    gtk::Label,                                         // net_status_label
    gtk::Box, gtk::Label, gtk::Button, gtk::Button,     // incoming consent bar
    gtk::Box, gtk::Label,                               // outgoing consent bar
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
        let lbl = gtk::Label::new(Some("Connected"));
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
    // (narrow window).  Uses a hamburger icon so it's visually familiar.
    // On non-macOS: placed on the left (start).
    // On macOS: placed on the right (end) to avoid the traffic-light buttons.
    let make_sidebar_btn = || {
        let btn = gtk::Button::from_icon_name("open-menu-symbolic");
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
    #[cfg(not(target_os = "macos"))]
    chart_header.pack_start(&chart_sidebar_btn);
    #[cfg(target_os = "macos")]
    chart_header.pack_end(&chart_sidebar_btn);
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
    #[cfg(not(target_os = "macos"))]
    sky_header.pack_start(&sky_sidebar_btn);
    #[cfg(target_os = "macos")]
    sky_header.pack_end(&sky_sidebar_btn);
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
    let stargazers_content = gtk::Box::new(gtk::Orientation::Vertical, 16);

    let net_status_label = gtk::Label::new(Some("Starting up…"));
    net_status_label.add_css_class("dim-label");
    net_status_label.add_css_class("caption");
    net_status_label.set_halign(gtk::Align::Center);
    net_status_label.set_margin_top(8);
    stargazers_content.append(&net_status_label);

    peers_clamp.set_child(Some(&stargazers_content));
    peers_scroll.set_child(Some(&peers_clamp));

    let network_header = adw::HeaderBar::new();
    network_header.set_title_widget(Some(&adw::WindowTitle::new("Network", "")));
    let network_sidebar_btn = make_sidebar_btn();
    #[cfg(not(target_os = "macos"))]
    network_header.pack_start(&network_sidebar_btn);
    #[cfg(target_os = "macos")]
    network_header.pack_end(&network_sidebar_btn);
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
    // On macOS put the sidebar on the right to avoid the traffic-light zone.
    #[cfg(target_os = "macos")]
    split_view.set_sidebar_position(gtk::PackType::End);

    // Burger button visibility is driven by the collapsed state.
    // The `collapsed` property itself is driven by an adw::Breakpoint attached
    // to the root window in build_widgets — that is where we have access to
    // the window and can register the breakpoint.
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
                            s.input(AppMsg::OpenStargazer(PeerId(arr)));
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

    // Consent request bar — shown when a peer wants to connect.
    let consent_bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    consent_bar.add_css_class("toolbar");
    consent_bar.set_margin_start(8);
    consent_bar.set_margin_end(8);
    consent_bar.set_visible(false);

    let consent_status = gtk::Label::new(None);
    consent_status.set_hexpand(true);
    consent_status.set_halign(gtk::Align::Start);
    consent_bar.append(&consent_status);

    let consent_accept_btn = gtk::Button::with_label("Connect  ✓");
    consent_accept_btn.add_css_class("suggested-action");
    consent_accept_btn.add_css_class("pill");
    let s = sender.clone();
    consent_accept_btn.connect_clicked(move |_| { s.input(AppMsg::AcceptConsent); });
    consent_bar.append(&consent_accept_btn);

    let consent_reject_btn = gtk::Button::with_label("Decline  ✕");
    consent_reject_btn.add_css_class("destructive-action");
    consent_reject_btn.add_css_class("pill");
    let s = sender.clone();
    consent_reject_btn.connect_clicked(move |_| { s.input(AppMsg::RejectConsent); });
    consent_bar.append(&consent_reject_btn);

    toolbar_view.add_bottom_bar(&consent_bar);

    // Call bar — shown during active/ringing/outgoing calls.
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

    // Outgoing consent bar — shown when the local user has staged a "+" proposal.
    let outgoing_consent_bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    outgoing_consent_bar.add_css_class("toolbar");
    outgoing_consent_bar.set_margin_start(8);
    outgoing_consent_bar.set_margin_end(8);
    outgoing_consent_bar.set_visible(false);

    let outgoing_consent_status = gtk::Label::new(None);
    outgoing_consent_status.set_hexpand(true);
    outgoing_consent_status.set_halign(gtk::Align::Start);
    outgoing_consent_bar.append(&outgoing_consent_status);

    let share_btn = gtk::Button::with_label("Share  ✓");
    share_btn.add_css_class("suggested-action");
    share_btn.add_css_class("pill");
    let s = sender.clone();
    share_btn.connect_clicked(move |_| s.input(AppMsg::ConfirmOutgoingConsent));
    outgoing_consent_bar.append(&share_btn);

    let cancel_outgoing_btn = gtk::Button::with_label("Cancel  ✕");
    cancel_outgoing_btn.add_css_class("pill");
    let s = sender.clone();
    cancel_outgoing_btn.connect_clicked(move |_| s.input(AppMsg::CancelOutgoingConsent));
    outgoing_consent_bar.append(&cancel_outgoing_btn);

    toolbar_view.add_bottom_bar(&outgoing_consent_bar);

    let _ = model;

    (
        toolbar_view,
        chart_container, sky_container,
        split_view, nav_list,
        content_stack, stargazers_content,
        notif_btn, notif_label,
        net_status_label,
        consent_bar, consent_status, consent_accept_btn, consent_reject_btn,
        outgoing_consent_bar, outgoing_consent_status,
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
fn load_stargazers(data_dir: &std::path::Path) -> HashMap<PeerId, zodia_net::ConsentBlob> {
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
            let blob = zodia_net::ConsentBlob {
                birth: zodia_core::BirthData::new(jdn, geohash),
                prekey:    [0u8; 32],
                ephemeral: [0u8; 32],
                relay_pk:  [0u8; 32],
            };
            Some((peer_id, blob))
        })
        .collect()
}

fn save_stargazers(data_dir: &std::path::Path, peers: &HashMap<PeerId, zodia_net::ConsentBlob>) {
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
