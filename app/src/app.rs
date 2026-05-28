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

use relm4::factory::FactoryVecDeque;

use crate::aspect_list;
use crate::aspect_view;
use crate::notify;
use crate::peer_row::{PeerRow, PeerRowInit, PeerRowMsg, PeerRowOut};
use crate::stargazer_list::{Stargazer, StargazerState};
use crate::stargazer_page::{self, append_chat_row};
use crate::util::{approximate_aspects, sign_glyph};

// ── init ──────────────────────────────────────────────────────────────────────

pub struct AppInit {
    pub config: LocalConfig,
    pub store_path: std::path::PathBuf,
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
    /// "+" on a discovered peer — immediately enters OutgoingPending and starts connecting.
    ProposeConsent(PeerId),
    /// "×" on a pending/connected sidebar row — remove from all state and disk.
    RemoveStargazer(PeerId),
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
    /// User tapped the affirm button on a community interpretation row.
    AffirmInterp { log_id: [u8; 32] },
    /// User submitted a fresh community interpretation from a detail page.
    SubmitInterp { key: InterpKey, body: String },
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct AppModel {
    on_setup_page: bool,
    chart: Option<Chart>,

    store: ZodiaStore,
    baseline: Rc<BaselineStore>,

    network: Option<Arc<ZodiaNetwork>>,
    node_id_text: String,

    /// All known stargazers in any state (Discovered → OutgoingPending →
    /// IncomingPending → Connected).  Connected peers are persisted in peers.tsv;
    /// OutgoingPending peers in pending.tsv; the rest are ephemeral.
    stargazers: HashMap<PeerId, Stargazer>,
    /// Active QUIC channels — presence means the channel is open.
    connected_channels: HashMap<PeerId, DirectChannel>,
    /// Explicit presence state received from each stargazer over their channel.
    stargazer_status: HashMap<PeerId, PeerStatus>,

    /// Incremented when peer state changes; drives the network-tab rebuild and
    /// the peer-page title sync in `update_view`.  The sidebar peer rows are
    /// kept fresh by `sync_peers_factory` directly and don't read this token.
    network_changed_token: u64,

    /// Factory-backed peer rows in the "Others" sidebar section.
    peers_factory: FactoryVecDeque<PeerRow>,

    /// Peers the user has explicitly tapped; pages pushed once Tier-1 completes.
    /// Uses `RefCell` for interior mutability inside `update_view (&self)`.
    pending_push_queue: RefCell<Vec<PeerId>>,

    config: LocalConfig,
    /// Sender for the SetupPage child component (only used while setup is shown).
    setup_sender: Option<relm4::Sender<crate::setup_page::SetupPageMsg>>,
    /// Sender for the sidebar NotifBell child component.
    notif_sender: Option<relm4::Sender<crate::notif_bell::NotifBellMsg>>,
    /// Sender for the Network tab child component.
    network_tab_sender: Option<relm4::Sender<crate::network_tab::NetworkTabMsg>>,
    /// Sender for the Sidebar child component.
    sidebar_sender: Option<relm4::Sender<crate::sidebar::SidebarMsg>>,

    identity: Rc<IdentityKeypair>,

    call_state: CallState,
    active_audio: Option<AudioSession>,

    /// Chat history per peer: `(from_us, text)`.
    chat_logs: HashMap<PeerId, Vec<(bool, String)>>,

    /// User-assigned nicknames, keyed by 4-byte upper-hex stargazer tag.
    stargazer_nicknames: HashMap<String, String>,
    /// Unread message counts per peer (cleared when their page is opened).
    unread_messages: HashMap<String, usize>,

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

    chart_container: gtk::Box,
    sky_container: gtk::Box,

    /// Overlay split view — sidebar on the left, content stack on the right.
    split_view: adw::OverlaySplitView,
    /// Generation of the network view / page titles we last rendered.
    network_changed_token_shown: u64,

    /// Single content stack — chart / sky / stargazers + per-stargazer pages, all as named children.
    content_stack: gtk::Stack,

    /// Message list widget per stargazer (keyed by 4-byte hex tag).
    stargazer_msg_lists: HashMap<String, gtk::ListBox>,
    /// How many messages from `chat_logs` have already been appended to each list.
    stargazer_chat_shown: HashMap<String, usize>,
    /// Call and send buttons per stargazer — disabled when stargazer is offline.
    stargazer_actions: HashMap<String, (gtk::Button, gtk::Button, gtk::Entry)>,
    /// ViewSwitcherTitle per stargazer — updated when the nickname changes.
    #[allow(deprecated)]
    stargazer_titles: HashMap<String, adw::ViewSwitcherTitle>,

    call_bar: gtk::Box,
    call_status: gtk::Label,
    accept_btn: gtk::Button,
    hangup_btn: gtk::Button,

    consent_bar: gtk::Box,
    consent_status: gtk::Label,
    consent_accept_btn: gtk::Button,
    consent_reject_btn: gtk::Button,
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
        let store = match ZodiaStore::open(&init.store_path).await {
            Ok(s) => s,
            Err(e) => {
                error!("fatal: could not open store: {e}");
                std::process::exit(1);
            }
        };
        match store.scrub_baseline().await {
            Ok(0) => {}
            Ok(n) => info!("scrubbed {n} legacy baseline rows from DB"),
            Err(e) => warn!("scrub_baseline failed: {e}"),
        }
        let baseline = Rc::new(init.baseline);

        let stargazer_nicknames = load_nicknames(init.config.data_dir());

        // Build unified stargazer map from both persisted connected peers and
        // saved outgoing-pending peers.
        let mut stargazers: HashMap<PeerId, Stargazer> = HashMap::new();
        for (peer_id, blob) in load_connected_stargazers(init.config.data_dir()) {
            stargazers.insert(peer_id.clone(), Stargazer {
                peer_id: peer_id.clone(),
                solar_month:         zodia_core::solar_month(blob.birth.jdn),
                geohash_prefix:      blob.birth.geohash.chars().take(3).collect(),
                approximate_aspects: Vec::new(),
                state:               StargazerState::Connected { birth: blob },
            });
        }
        for peer_id in load_pending(init.config.data_dir()) {
            stargazers.entry(peer_id.clone())
                .or_insert_with(|| Stargazer::outgoing_pending(peer_id));
        }

        // Pre-load chat history for all persisted (Connected) stargazers.
        let mut chat_logs: HashMap<PeerId, Vec<(bool, String)>> = HashMap::new();
        for s in stargazers.values()
            .filter(|s| matches!(s.state, StargazerState::Connected { .. }))
        {
            if let Ok(msgs) = store.messages_for_peer(&s.peer_id.0).await {
                if !msgs.is_empty() {
                    chat_logs.insert(s.peer_id.clone(), msgs);
                }
            }
        }

        // Create the peer-row factory before building widgets so we can embed its
        // gtk::ListBox in the sidebar layout.
        let peers_factory: FactoryVecDeque<PeerRow> = FactoryVecDeque::builder()
            .launch({
                let l = gtk::ListBox::new();
                l.add_css_class("navigation-sidebar");
                // Each PeerRow's widget_name is "{bucket:02}_{peer_hex}".  Lexical
                // compare gives the right order (bucket first, peer hex tiebreak).
                l.set_sort_func(|a, b| {
                    a.widget_name().cmp(&b.widget_name()).into()
                });
                l
            })
            .forward(sender.input_sender(), |out| match out {
                PeerRowOut::Activate(pid)  => AppMsg::OpenStargazer(pid),
                PeerRowOut::Remove(pid)    => AppMsg::RemoveStargazer(pid),
                PeerRowOut::SetNickname { peer_id, name } => AppMsg::SetNickname { peer_id, name },
            });

        let mut model = AppModel {
            on_setup_page: !has_birth,
            chart: None,
            store,
            baseline,
            network: None,
            node_id_text: String::new(),
            stargazers,
            connected_channels: HashMap::new(),
            stargazer_status: HashMap::new(),
            network_changed_token: 0,
            peers_factory,
            pending_push_queue: RefCell::new(Vec::new()),
            config: init.config,
            setup_sender: None,
            notif_sender: None,
            network_tab_sender: None,
            sidebar_sender: None,
            identity,
            call_state: CallState::Idle,
            active_audio: None,
            chat_logs,
            stargazer_nicknames,
            unread_messages: HashMap::new(),
            sync_publish_tx: None,
            recent_interps: Vec::new(),
        };
        // Populate the factory with persisted peers (Connected + OutgoingPending).
        sync_peers_factory(&mut model);
        model.recent_interps = model.store
            .recent_community_interps(12).await.unwrap_or_default();

        if let Some(birth) = model.config.birth.clone() {
            if let Ok(chart) = Chart::compute(birth.clone()) {
                model.chart = Some(chart);
            }
        }

        // Launch SetupPage child component; output forwards to AppMsg.
        let (setup_widget, setup_sender) = crate::setup_page::launch(
            sender.input_sender(),
            |out| match out {
                crate::setup_page::SetupPageOut::Submit { year, month, day, hour, minute, lat, lon } =>
                    AppMsg::ConfirmBirth { year, month, day, hour, minute, lat, lon },
                crate::setup_page::SetupPageOut::Error(e) =>
                    AppMsg::SetupError(e),
            },
        );
        model.setup_sender = Some(setup_sender);

        // Launch NotifBell child component (sidebar header).
        let (notif_widget, notif_sender) = crate::notif_bell::launch();
        model.notif_sender = Some(notif_sender);

        let (widgets, network_tab_sender, sidebar_sender) =
            build_widgets(&root, &model, &sender, &setup_widget, &notif_widget);
        model.network_tab_sender = Some(network_tab_sender);
        model.sidebar_sender = Some(sidebar_sender);

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
                self.on_setup_page = false;
            }

            AppMsg::SetupError(msg) => {
                if let Some(s) = &self.setup_sender {
                    let _ = s.send(crate::setup_page::SetupPageMsg::SetError(msg));
                }
            }

            AppMsg::NetworkReady => {
                // After a short settle delay, attempt to reconnect every persisted
                // peer (Connected offline) and retry every OutgoingPending peer.
                let to_reconnect: Vec<PeerId> = self.stargazers.values()
                    .filter(|s| matches!(
                        s.state,
                        StargazerState::Connected { .. } | StargazerState::OutgoingPending
                    ))
                    .map(|s| s.peer_id.clone())
                    .collect();
                if !to_reconnect.is_empty() {
                    let s = sender.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(4)).await;
                        for peer_id in to_reconnect {
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
                // Determine what kind of connection attempt to make.
                let is_new = match self.stargazers.get(&peer_id).map(|s| &s.state) {
                    Some(StargazerState::Connected { .. })
                        if !self.connected_channels.contains_key(&peer_id) => false,
                    Some(StargazerState::OutgoingPending) => true,
                    _ => return,
                };
                let (Some(net), Some(our_blob)) = (
                    self.network.as_ref(),
                    make_consent_blob(&self.config, &self.identity),
                ) else { return };
                let net = Arc::clone(net);
                let pid = peer_id.clone();
                let peer_hex = hex::encode_upper(&pid.0[..4]);
                let s = sender.clone();
                info!(peer = %peer_hex, is_new, "attempting connection");
                tokio::spawn(async move {
                    match net.connect_peer(&pid).await {
                        Ok(channel) => {
                            match channel.exchange_consent(&our_blob).await {
                                Ok(their_blob) => {
                                    info!(peer = %peer_hex, "consent exchange ok");
                                    s.input(AppMsg::ConnectionComplete {
                                        peer_id: pid,
                                        their_blob,
                                        channel,
                                        navigate: false,
                                        is_new,
                                    });
                                }
                                Err(e) => warn!(peer = %peer_hex, "consent exchange: {e}"),
                            }
                        }
                        Err(e) => warn!(peer = %peer_hex, "connect_peer failed: {e}"),
                    }
                });
            }

            AppMsg::SetNickname { peer_id, name } => {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                if name.trim().is_empty() {
                    self.stargazer_nicknames.remove(&tag);
                } else {
                    self.stargazer_nicknames.insert(tag, name.trim().to_string());
                }
                save_nicknames(self.config.data_dir(), &self.stargazer_nicknames);
                self.network_changed_token += 1;
                sync_peers_factory(self);
            }

            AppMsg::ProposeConsent(peer_id) => {
                // Idempotent: skip if already Connected or already seeking.
                match self.stargazers.get(&peer_id).map(|s| &s.state) {
                    Some(StargazerState::Connected { .. })
                    | Some(StargazerState::OutgoingPending) => return,
                    _ => {}
                }
                if let Some(s) = self.stargazers.get_mut(&peer_id) {
                    s.state = StargazerState::OutgoingPending;
                } else {
                    self.stargazers.insert(peer_id.clone(), Stargazer::outgoing_pending(peer_id.clone()));
                }
                save_pending(self.config.data_dir(), &self.stargazers);
                self.network_changed_token += 1;
                sync_peers_factory(self);
                // Immediately kick off the connection attempt.
                sender.input(AppMsg::Reconnect(peer_id));
            }

            AppMsg::RemoveStargazer(peer_id) => {
                let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                if let Some(removed) = self.stargazers.remove(&peer_id) {
                    match removed.state {
                        StargazerState::IncomingPending { .. } => {
                            notify::withdraw(&format!("consent-{peer_hex}"));
                            // Dropping the channel closes the QUIC connection.
                        }
                        StargazerState::OutgoingPending => {
                            save_pending(self.config.data_dir(), &self.stargazers);
                        }
                        StargazerState::Connected { .. } => {
                            self.connected_channels.remove(&peer_id);
                            self.stargazer_status.remove(&peer_id);
                            save_stargazers(self.config.data_dir(), &self.stargazers);
                        }
                        StargazerState::Discovered => {}
                    }
                }
                self.network_changed_token += 1;
                sync_peers_factory(self);
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
                }
                // Always exchange the relevant-to-this-pair community
                // interpretations on every successful (re)connect, not just
                // the first one — otherwise long-lived peers stop sharing
                // their newly-authored entries with each other after their
                // initial handshake.
                let imported = do_interp_sync(
                    &channel, &their_blob,
                    self.chart.as_ref(), &self.store,
                    &self.identity, &peer_hex,
                ).await;
                if imported > 0 {
                    self.recent_interps = self.store
                        .recent_community_interps(12)
                        .await
                        .unwrap_or_default();
                }
                // Transition to Connected (update in place, preserve announce info).
                if let Some(s) = self.stargazers.get_mut(&peer_id) {
                    s.state = StargazerState::Connected { birth: their_blob };
                } else {
                    // Arrived before any PeerDiscovered (e.g. incoming auto-accepted).
                    self.stargazers.insert(peer_id.clone(), Stargazer {
                        peer_id: peer_id.clone(),
                        solar_month:         zodia_core::solar_month(their_blob.birth.jdn),
                        geohash_prefix:      their_blob.birth.geohash.chars().take(3).collect(),
                        approximate_aspects: Vec::new(),
                        state:               StargazerState::Connected { birth: their_blob },
                    });
                }
                if is_new {
                    save_stargazers(self.config.data_dir(), &self.stargazers);
                    save_pending(self.config.data_dir(), &self.stargazers);
                }
                if let Some(net) = &self.network {
                    net.accept_channel(peer_id.clone(), channel.clone());
                }
                send_status_active(&channel);
                self.connected_channels.insert(peer_id.clone(), channel);
                self.network_changed_token += 1;
                sync_peers_factory(self);
                if navigate {
                    self.pending_push_queue.borrow_mut().push(peer_id);
                }
            }

            AppMsg::OpenStargazer(peer_id) => {
                // Only Connected peers have a navigable page; pending rows are
                // not activatable so this should only be called for Connected peers.
                let tag = hex::encode_upper(&peer_id.0[..4]);
                self.unread_messages.remove(&tag);
                self.pending_push_queue.borrow_mut().push(peer_id);
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
                let incoming_id = self.stargazers.iter()
                    .find(|(_, s)| matches!(s.state, StargazerState::IncomingPending { .. }))
                    .map(|(id, _)| id.clone());
                let Some(peer_id) = incoming_id else { return };
                let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                notify::withdraw(&format!("consent-{peer_hex}"));
                let channel = match &self.stargazers[&peer_id].state {
                    StargazerState::IncomingPending { channel } => channel.clone(),
                    _ => return,
                };
                let Some(our_blob) = make_consent_blob(&self.config, &self.identity) else {
                    if let Some(s) = self.stargazers.get_mut(&peer_id) {
                        s.state = StargazerState::Discovered;
                    }
                    self.network_changed_token += 1;
                    sync_peers_factory(self);
                    return;
                };
                match channel.exchange_consent(&our_blob).await {
                    Ok(their_blob) => {
                        info!(peer = %peer_hex, "consent exchange complete");
                        let imported = do_interp_sync(
                            &channel, &their_blob,
                            self.chart.as_ref(), &self.store,
                            &self.identity, &peer_hex,
                        ).await;
                        if imported > 0 {
                            self.recent_interps = self.store
                                .recent_community_interps(12)
                                .await
                                .unwrap_or_default();
                        }
                        if let Some(s) = self.stargazers.get_mut(&peer_id) {
                            s.state = StargazerState::Connected { birth: their_blob };
                        }
                        save_stargazers(self.config.data_dir(), &self.stargazers);
                    }
                    Err(e) => {
                        warn!(peer = %peer_hex, "consent exchange failed: {e}");
                        if let Some(s) = self.stargazers.get_mut(&peer_id) {
                            s.state = StargazerState::Discovered;
                        }
                    }
                }
                if let Some(net) = &self.network {
                    net.accept_channel(peer_id.clone(), channel.clone());
                }
                send_status_active(&channel);
                self.connected_channels.insert(peer_id, channel);
                self.network_changed_token += 1;
                sync_peers_factory(self);
            }

            AppMsg::RejectConsent => {
                let incoming_id = self.stargazers.iter()
                    .find(|(_, s)| matches!(s.state, StargazerState::IncomingPending { .. }))
                    .map(|(id, _)| id.clone());
                if let Some(peer_id) = incoming_id {
                    let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                    notify::withdraw(&format!("consent-{peer_hex}"));
                    info!(peer = %peer_hex, "consent request declined");
                    if let Some(s) = self.stargazers.get_mut(&peer_id) {
                        s.state = StargazerState::Discovered;
                    }
                    self.network_changed_token += 1;
                    sync_peers_factory(self);
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
                        let _ = self.store.insert_message(&peer_id.0, true, &text).await;
                        self.chat_logs.entry(peer_id).or_default().push((true, text));
                    }
                }
            }

            AppMsg::SendViaRelay { relay, dest, text } => {
                let Some(relay_channel) = self.connected_channels.get(&relay) else { return };
                let Some(their_blob) = self.stargazers.get(&dest).and_then(|s| match &s.state {
                    StargazerState::Connected { birth } => Some(birth),
                    _ => None,
                }) else { return };
                let our_id = self.network.as_ref().map(|n| n.node_id()).unwrap_or(PeerId([0u8; 32]));

                let inner = RelayPayload { from: our_id.0, text: text.clone() };
                let mut cbor = Vec::new();
                if ciborium::into_writer(&inner, &mut cbor).is_err() { return };
                let payload = ecies_encrypt(&their_blob.relay_pk, &cbor);
                let msg = ChannelMsg::RelayMsg { dest: dest.0, payload };
                if relay_channel.send_msg(&msg).await.is_ok() {
                    let _ = self.store.insert_message(&dest.0, true, &text).await;
                    self.chat_logs.entry(dest).or_default().push((true, text));
                }
            }
            AppMsg::ShareInterp(entry) => {
                // Reload activity feed (insert_signed already ran in aspect_view).
                self.recent_interps = self.store
                    .recent_community_interps(12).await.unwrap_or_default();
                self.network_changed_token += 1;
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
                self.recent_interps = self.store
                    .recent_community_interps(12).await.unwrap_or_default();
                self.network_changed_token += 1;
            }
            AppMsg::AffirmInterp { log_id } => {
                let author_pk = self.identity.public_key();
                match self.store.affirm(&log_id, &author_pk).await {
                    Ok(_) => self.network_changed_token += 1,
                    Err(e) => warn!("affirm failed: {e}"),
                }
            }
            AppMsg::SubmitInterp { key, body } => {
                let payload    = ZodiaStore::signing_payload(&key, &body);
                let author_sig = self.identity.sign(&payload);
                let author_pk  = self.identity.public_key();
                match self.store
                    .insert_signed(&key, &body, &author_pk, &author_sig)
                    .await
                {
                    Ok(_) => {
                        sender.input(AppMsg::ShareInterp(InterpEntry {
                            interp_key: key.to_sig(),
                            body,
                            author_pk,
                            author_sig: author_sig.to_vec(),
                        }));
                    }
                    Err(e) => warn!("insert_signed failed: {e}"),
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
                match self.stargazers.get_mut(&peer_id) {
                    Some(s) => {
                        // Update announce info regardless of current state.
                        s.solar_month         = blob.solar_month;
                        s.geohash_prefix      = blob.geohash_prefix.clone();
                        s.approximate_aspects = approx;
                        // If we want to reach them (OutgoingPending) OR they're
                        // a previously-connected peer who's just come back
                        // online without our active channel, retry now.  This
                        // is how reconnects happen after one side restarts —
                        // we see the announce, we initiate.
                        let should_reconnect = matches!(
                            s.state,
                            StargazerState::OutgoingPending
                        ) || (
                            matches!(s.state, StargazerState::Connected { .. })
                                && !self.connected_channels.contains_key(&peer_id)
                        );
                        if should_reconnect {
                            _sender.input(AppMsg::Reconnect(peer_id.clone()));
                        }
                    }
                    None => {
                        self.stargazers.insert(
                            peer_id.clone(),
                            Stargazer::discovered(peer_id, &blob, approx),
                        );
                    }
                }
                self.network_changed_token += 1;
                sync_peers_factory(self);
                // Re-publish our announce so the newly-seen peer can discover us too.
                if let Some(net) = &self.network {
                    if let Err(e) = net.publish_announce().await {
                        warn!("re-announce on peer-discovered failed: {e}");
                    }
                }
            }
            ZodiaNetEvent::PeerLeft { peer_id } => {
                match self.stargazers.get(&peer_id).map(|s| &s.state) {
                    Some(StargazerState::Discovered) | None => {
                        self.stargazers.remove(&peer_id);
                    }
                    Some(StargazerState::IncomingPending { .. }) => {
                        // Their channel is gone; revert so they don't linger as pending.
                        if let Some(s) = self.stargazers.get_mut(&peer_id) {
                            s.state = StargazerState::Discovered;
                        }
                    }
                    // OutgoingPending stays (retry on next PeerDiscovered).
                    // Connected stays (reconnect loop handles it).
                    _ => {}
                }
                self.network_changed_token += 1;
                sync_peers_factory(self);
            }
            ZodiaNetEvent::IncomingChannel { peer_id, channel } => {
                let peer_hex = hex::encode_upper(&peer_id.0[..4]);

                // Mutual-pending fast path: if we're already seeking them, skip
                // the consent bar and auto-accept the exchange.
                if matches!(
                    self.stargazers.get(&peer_id).map(|s| &s.state),
                    Some(StargazerState::OutgoingPending)
                ) {
                    info!(peer = %peer_hex, "mutual pending — auto-accepting incoming channel");
                    if let Some(our_blob) = make_consent_blob(&self.config, &self.identity) {
                        let s = _sender.clone();
                        let pid = peer_id.clone();
                        tokio::spawn(async move {
                            match channel.exchange_consent(&our_blob).await {
                                Ok(their_blob) => {
                                    s.input(AppMsg::ConnectionComplete {
                                        peer_id: pid,
                                        their_blob,
                                        channel,
                                        navigate: false,
                                        is_new: true,
                                    });
                                }
                                Err(e) => warn!(peer = %peer_hex, "mutual auto-accept failed: {e}"),
                            }
                        });
                    }
                    return;
                }

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
                // Update or insert the IncomingPending state.
                match self.stargazers.get_mut(&peer_id) {
                    Some(s) => s.state = StargazerState::IncomingPending { channel },
                    None    => {
                        self.stargazers.insert(peer_id.clone(), Stargazer {
                            peer_id,
                            solar_month:         0,
                            geohash_prefix:      String::new(),
                            approximate_aspects: Vec::new(),
                            state:               StargazerState::IncomingPending { channel },
                        });
                    }
                }
                self.network_changed_token += 1;
                sync_peers_factory(self);
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
                let _ = self.store.insert_message(&from.0, false, &text).await;
                self.chat_logs.entry(from).or_default().push((false, text));
            }
            ZodiaNetEvent::PeerStatusChanged { peer_id, status } => {
                let tag = hex::encode_upper(&peer_id.0[..4]);
                info!(peer = %tag, ?status, "peer status update");
                self.stargazer_status.insert(peer_id, status);
                self.network_changed_token += 1;
                sync_peers_factory(self);
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
                                    let _ = self.store.insert_message(&from.0, false, &rp.text).await;
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
                self.network_changed_token += 1;
                sync_peers_factory(self);
                // Schedule a reconnect attempt for Connected peers after 10 s.
                if matches!(
                    self.stargazers.get(&peer_id).map(|s| &s.state),
                    Some(StargazerState::Connected { .. })
                ) {
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
                let n = import_interps(&entries, &self.store, &peer_hex).await;
                if n > 0 {
                    info!(peer = %peer_hex, "imported {n} live interpretations from peer");
                    // Refresh the network tab's activity feed and any open
                    // detail pages so the newly imported entries are visible
                    // without needing a manual refresh.
                    self.recent_interps = self.store
                        .recent_community_interps(12)
                        .await
                        .unwrap_or_default();
                    self.network_changed_token += 1;
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

        // ── lazily populate aspect views ──────────────────────────────────────

        if !self.on_setup_page && widgets.chart_container.first_child().is_none() {
            if let Some(chart) = &self.chart {
                let nav = aspect_view::launch(aspect_view::AspectViewInit {
                    kind:             aspect_view::AspectViewKind::Natal,
                    items:            aspect_list::natal_items(&chart.natal_aspects()),
                    placements_items: crate::placements::placement_items(chart),
                    chart:            None,
                    store:            self.store.clone(),
                    baseline:         Rc::clone(&self.baseline),
                    identity:         Rc::clone(&self.identity),
                    parent_sender:    sender.clone(),
                });
                nav.set_vexpand(true);
                widgets.chart_container.append(&nav);

                if let Ok(ts) = chart.transits_at(current_jdn()) {
                    let tav = aspect_view::launch(aspect_view::AspectViewInit {
                        kind:             aspect_view::AspectViewKind::Transit,
                        items:            aspect_list::transit_items(
                            &ts.transit_aspects,
                            &ts.house_transits,
                            &chart.positions,
                            ts.transit_jdn,
                        ),
                        placements_items: vec![],
                        chart:            None,
                        store:            self.store.clone(),
                        baseline:         Rc::clone(&self.baseline),
                        identity:         Rc::clone(&self.identity),
                        parent_sender:    sender.clone(),
                    });
                    tav.set_vexpand(true);
                    widgets.sky_container.append(&tav);
                }
            }
        }

        // ── factory "Others" header visibility ────────────────────────────────

        if let Some(s) = &self.sidebar_sender {
            let _ = s.send(crate::sidebar::SidebarMsg::SetOthersVisible(
                !self.peers_factory.is_empty()
            ));
        }

        // ── rebuild network view and sync page titles when content changes ────

        if self.network_changed_token != widgets.network_changed_token_shown {
            send_network_refresh(self);
            // Keep open peer page titles in sync with nicknames.
            for s in self.stargazers.values()
                .filter(|s| matches!(s.state, StargazerState::Connected { .. }))
            {
                let peer_hex = hex::encode_upper(&s.peer_id.0[..4]);
                let glyph    = if s.solar_month > 0 { sign_glyph(s.solar_month) } else { "" };
                #[allow(deprecated)]
                if let Some(tw) = widgets.stargazer_titles.get(&peer_hex) {
                    let title = self.stargazer_nicknames.get(&peer_hex)
                        .filter(|n| !n.is_empty())
                        .map(|n| format!("{glyph}  {n}"))
                        .unwrap_or_else(|| format!("{glyph}  ···{peer_hex}"));
                    tw.set_title(&title);
                }
            }
            widgets.network_changed_token_shown = self.network_changed_token;
        }

        // ── push peer pages for OpenPeer requests ─────────────────────────────

        let pending: Vec<PeerId> = self.pending_push_queue.borrow_mut().drain(..).collect();
        if !pending.is_empty() {
            // Opening a peer page → clear nav-list selection so Chart/Sky/Network
            // doesn't stay highlighted alongside the active peer row.
            if let Some(s) = &self.sidebar_sender {
                let _ = s.send(crate::sidebar::SidebarMsg::UnselectNav);
            }
        }
        for peer_id in pending {
            let their_blob = self.stargazers.get(&peer_id).and_then(|s| match &s.state {
                StargazerState::Connected { birth } => Some(birth),
                _ => None,
            });
            if let Some(their_blob) = their_blob {
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
                                self.store.clone(),
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

        if let Some(s) = &self.network_tab_sender {
            let connected = self.stargazers.values()
                .filter(|s| matches!(s.state, StargazerState::Connected { .. }))
                .count();
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
            let _ = s.send(crate::network_tab::NetworkTabMsg::SetStatus(text));
        }

        // ── notification bell ─────────────────────────────────────────────────

        if let Some(s) = &self.notif_sender {
            let total_unread: usize = self.unread_messages.values().sum();
            let summary = if total_unread > 0 {
                self.unread_messages.iter()
                    .filter(|(_, &n)| n > 0)
                    .map(|(tag, n)| {
                        let name = self.stargazer_nicknames.get(tag)
                            .cloned()
                            .unwrap_or_else(|| format!("···{tag}"));
                        format!("{name}  ·  {n} unread")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                String::new()
            };
            let _ = s.send(crate::notif_bell::NotifBellMsg::Set { summary, total_unread });
        }

        // ── consent bar ──────────────────────────────────────────────────────

        let first_incoming = self.stargazers.values()
            .find(|s| matches!(s.state, StargazerState::IncomingPending { .. }));
        if let Some(s) = first_incoming {
            let tag = hex::encode_upper(&s.peer_id.0[..4]);
            let glyph = if s.solar_month > 0 { sign_glyph(s.solar_month).to_string() } else { String::new() };
            let more = self.stargazers.values()
                .filter(|s2| matches!(s2.state, StargazerState::IncomingPending { .. }))
                .count()
                .saturating_sub(1);
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

// ── factory peer sync ─────────────────────────────────────────────────────────

/// Compute the `PeerRowInit` for a stargazer (must not be Discovered).
fn make_peer_row_init(s: &Stargazer, model: &AppModel) -> PeerRowInit {
    let peer_hex    = hex::encode_upper(&s.peer_id.0[..4]);
    let has_channel = model.connected_channels.contains_key(&s.peer_id);
    let status      = model.stargazer_status.get(&s.peer_id);
    let is_connected = matches!(s.state, StargazerState::Connected { .. });
    let is_pending   = matches!(
        s.state,
        StargazerState::OutgoingPending | StargazerState::IncomingPending { .. }
    );
    let (dot_filled, dot_rgba) = match &s.state {
        StargazerState::IncomingPending { .. } =>
            (true,  [0.95_f32, 0.75, 0.30, 1.0]),
        StargazerState::OutgoingPending =>
            (false, [0.55_f32, 0.55, 0.55, 0.55]),
        StargazerState::Connected { .. } => match (has_channel, status) {
            (_, Some(PeerStatus::Active)) => (true,  [0.46_f32, 0.82, 0.46, 1.0]),
            (_, Some(PeerStatus::Away))   => (true,  [0.95,     0.75, 0.30, 1.0]),
            (true,  None)                 => (true,  [0.95,     0.75, 0.30, 1.0]),
            (false, _)                    => (false, [0.55,     0.55, 0.55, 0.70]),
        },
        StargazerState::Discovered => unreachable!(),
    };
    let display_name = if is_connected {
        model.stargazer_nicknames.get(&peer_hex)
            .cloned()
            .unwrap_or_else(|| format!("···{peer_hex}"))
    } else {
        format!("···{peer_hex}")
    };
    let nickname = model.stargazer_nicknames.get(&peer_hex).cloned().unwrap_or_default();
    let unread   = model.unread_messages.get(&peer_hex).copied().unwrap_or(0);

    PeerRowInit {
        peer_id: s.peer_id.clone(),
        solar_month: s.solar_month,
        display_name,
        is_connected,
        is_pending,
        sort_bucket: s.sidebar_sort_key(has_channel),
        dot_filled,
        dot_rgba,
        unread,
        nickname,
    }
}

/// Sync the factory-backed peer list with the current model state.
///
/// Set-diff against the current factory contents:
///   - peers in factory but not desired  → `guard.remove`
///   - peers in desired but not factory  → `guard.push_back`
///   - peers in both                     → `send(PeerRowMsg::Update)` in place
///
/// Visual order is handled by `peers_list.set_sort_func`, which reads each row's
/// `widget_name` (set by `PeerRow::update_view`).  After any structural or bucket
/// change we call `invalidate_sort` so GTK re-evaluates row positions.
fn sync_peers_factory(model: &mut AppModel) {
    // Build desired init map keyed by peer id.
    let mut desired: HashMap<PeerId, PeerRowInit> = model.stargazers.values()
        .filter(|s| !matches!(s.state, StargazerState::Discovered))
        .map(|s| (s.peer_id.clone(), make_peer_row_init(s, model)))
        .collect();

    // Walk current factory rows: either consume a desired entry (update) or mark for removal.
    let len = model.peers_factory.len();
    let mut to_update: Vec<(usize, PeerRowInit)> = Vec::new();
    let mut to_remove: Vec<usize> = Vec::new();
    for i in 0..len {
        let row = match model.peers_factory.get(i) {
            Some(r) => r,
            None    => continue,
        };
        if let Some(init) = desired.remove(&row.peer_id) {
            to_update.push((i, init));
        } else {
            to_remove.push(i);
        }
    }

    // Apply updates first — indices are still valid pre-removal.
    for (i, init) in to_update {
        model.peers_factory.send(i, PeerRowMsg::Update(Box::new(init)));
    }

    // Remove gone peers (reverse so earlier indices stay valid).
    if !to_remove.is_empty() {
        let mut guard = model.peers_factory.guard();
        for i in to_remove.into_iter().rev() {
            guard.remove(i);
        }
    }

    // Append any peers we hadn't seen yet.
    if !desired.is_empty() {
        let mut guard = model.peers_factory.guard();
        for (_, init) in desired {
            guard.push_back(init);
        }
    }

    // Re-evaluate sort order — buckets may have changed for updated rows.
    model.peers_factory.widget().invalidate_sort();
}

// ── network content view builder ──────────────────────────────────────────────

/// Ship a snapshot of discoverable peers + recent community interps to the
/// `NetworkTab` child component, which owns the Network tab UI internally.
fn send_network_refresh(model: &AppModel) {
    let Some(s) = &model.network_tab_sender else { return };

    let peers: Vec<crate::network_tab::NetPeer> = model.stargazers.values()
        .filter(|sg| matches!(sg.state, StargazerState::Discovered))
        .map(|sg| crate::network_tab::NetPeer {
            peer_id:        sg.peer_id.clone(),
            solar_month:    sg.solar_month,
            aspects:        sg.approximate_aspects.clone(),
            geohash_prefix: sg.geohash_prefix.clone(),
        })
        .collect();

    let recent: Vec<crate::network_tab::NetInterp> = model.recent_interps.iter()
        .map(|i| crate::network_tab::NetInterp {
            interp_key: i.interp_key.clone(),
            body:       i.body.clone(),
        })
        .collect();

    let _ = s.send(crate::network_tab::NetworkTabMsg::Refresh { peers, recent });
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
    use zodia_core::topic_key_global;

    let store_path = config.data_dir().join("interpretations.db");
    let sync_store = match ZodiaStore::open(&store_path).await {
        Ok(s) => s,
        Err(e) => { warn!("sync store open failed: {e}"); return None; }
    };

    let signing_key = config.identity.to_panda_key();
    let topic = p2panda_core::Topic::from(topic_key_global().0);

    let node = match ZodiaSyncNode::spawn(
        signing_key,
        net.endpoint(),
        net.gossip(),
        sync_store,
        topic,
        config.data_dir(),
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

/// Run a Tier-1 interpretation exchange.  Returns the count of *newly stored*
/// entries received from the peer so the caller can refresh activity feeds.
async fn do_interp_sync(
    channel: &DirectChannel,
    their_blob: &ConsentBlob,
    our_chart: Option<&Chart>,
    store: &ZodiaStore,
    identity: &Rc<IdentityKeypair>,
    peer_hex: &str,
) -> usize {
    let outgoing = collect_entries_for_stargazer(their_blob, our_chart, store, identity).await;
    match channel.exchange_interps(&outgoing).await {
        Ok(received) => {
            let n = import_interps(&received, store, peer_hex).await;
            if n > 0 {
                info!(peer = %peer_hex, "imported {n} interpretations from peer");
            }
            n
        }
        Err(e) => {
            warn!(peer = %peer_hex, "interp sync failed: {e}");
            0
        }
    }
}

async fn collect_entries_for_stargazer(
    their_blob: &ConsentBlob,
    our_chart: Option<&Chart>,
    store: &ZodiaStore,
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
    store.community_for_keys(&refs, 100).await
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

async fn import_interps(
    entries: &[InterpEntry],
    store: &ZodiaStore,
    peer_hex: &str,
) -> usize {
    let mut count = 0;
    for entry in entries {
        let Ok(sig_arr): Result<[u8; 64], _> = entry.author_sig.as_slice().try_into() else {
            warn!(peer = %peer_hex, key = %entry.interp_key, "invalid sig length, skipping");
            continue;
        };
        match store.insert_received(
            &entry.interp_key, &entry.body, &entry.author_pk, &sig_arr,
        ).await {
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
    setup_widget: &adw::ToolbarView,
    notif_widget: &gtk::MenuButton,
) -> (
    AppWidgets,
    relm4::Sender<crate::network_tab::NetworkTabMsg>,
    relm4::Sender<crate::sidebar::SidebarMsg>,
) {
    root.set_default_size(800, 620);

    let outer_stack = gtk::Stack::new();
    outer_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    outer_stack.set_transition_duration(200);

    outer_stack.add_named(setup_widget, Some("setup"));

    let (
        main_view,
        chart_container, sky_container,
        split_view,
        content_stack,
        network_tab_sender,
        sidebar_sender,
        consent_bar, consent_status, consent_accept_btn, consent_reject_btn,
        call_bar, call_status, accept_btn, hangup_btn,
    ) = build_main_page(model, sender, model.peers_factory.widget(), notif_widget);
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
        let nav = aspect_view::launch(aspect_view::AspectViewInit {
            kind:             aspect_view::AspectViewKind::Natal,
            items:            aspect_list::natal_items(&chart.natal_aspects()),
            placements_items: crate::placements::placement_items(chart),
            chart:            None,
            store:            model.store.clone(),
            baseline:         Rc::clone(&model.baseline),
            identity:         Rc::clone(&model.identity),
            parent_sender:    sender.clone(),
        });
        nav.set_vexpand(true);
        chart_container.append(&nav);

        if let Ok(ts) = chart.transits_at(current_jdn()) {
            let tav = aspect_view::launch(aspect_view::AspectViewInit {
                kind:             aspect_view::AspectViewKind::Transit,
                items:            aspect_list::transit_items(
                    &ts.transit_aspects,
                    &ts.house_transits,
                    &chart.positions,
                    ts.transit_jdn,
                ),
                placements_items: vec![],
                chart:            None,
                store:            model.store.clone(),
                baseline:         Rc::clone(&model.baseline),
                identity:         Rc::clone(&model.identity),
                parent_sender:    sender.clone(),
            });
            tav.set_vexpand(true);
            sky_container.append(&tav);
        }
    }

    outer_stack.set_visible_child_name(
        if model.on_setup_page { "setup" } else { "main" }
    );
    root.set_content(Some(&outer_stack));

    let widgets = AppWidgets {
        outer_stack,
        chart_container,
        sky_container,
        split_view,
        network_changed_token_shown: u64::MAX, // force initial network view build
        content_stack,
        stargazer_msg_lists: HashMap::new(),
        stargazer_chat_shown: HashMap::new(),
        stargazer_actions: HashMap::new(),
        stargazer_titles: HashMap::new(),
        consent_bar,
        consent_status,
        consent_accept_btn,
        consent_reject_btn,
        call_bar,
        call_status,
        accept_btn,
        hangup_btn,
    };
    (widgets, network_tab_sender, sidebar_sender)
}

// ── main page ─────────────────────────────────────────────────────────────────

/// Build a content-tab toolbar (header + body wrapped in `adw::ToolbarView`).
/// The returned sidebar button is hidden by default; visibility is driven by
/// the split-view collapsed state from `build_main_page`.
fn make_tab_toolbar(title: &str, body: &impl IsA<gtk::Widget>) -> (adw::ToolbarView, gtk::Button) {
    relm4::view! {
        sidebar_btn = gtk::Button {
            set_icon_name: "open-menu-symbolic",
            set_tooltip_text: Some("Show sidebar"),
            set_visible: false,
        }
    }
    relm4::view! {
        header = adw::HeaderBar {
            #[wrap(Some)]
            set_title_widget = &adw::WindowTitle::new(title, "") {},
        }
    }
    #[cfg(not(target_os = "macos"))]
    header.pack_start(&sidebar_btn);
    #[cfg(target_os = "macos")]
    header.pack_end(&sidebar_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(body));
    (toolbar, sidebar_btn)
}

#[allow(clippy::type_complexity)]
fn build_main_page(
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
    peers_list: &gtk::ListBox,
    notif_widget: &gtk::MenuButton,
) -> (
    adw::ToolbarView,                                   // outermost wrapper
    gtk::Box, gtk::Box,                                 // chart_container, sky_container
    adw::OverlaySplitView,                              // split_view
    gtk::Stack,                                         // content_stack
    relm4::Sender<crate::network_tab::NetworkTabMsg>,    // network_tab_sender
    relm4::Sender<crate::sidebar::SidebarMsg>,           // sidebar_sender
    gtk::Box, gtk::Label, gtk::Button, gtk::Button,     // incoming consent bar
    gtk::Box, gtk::Label, gtk::Button, gtk::Button,     // call bar
) {
    // ── Content area — single crossfade Stack for all views ──────────────────

    let content_stack = gtk::Stack::new();
    content_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    content_stack.set_transition_duration(150);

    // ── Overlay split view (constructed early so children can capture it) ────

    let split_view = adw::OverlaySplitView::new();
    split_view.set_content(Some(&content_stack));
    split_view.set_min_sidebar_width(200.0);
    split_view.set_max_sidebar_width(280.0);
    // On macOS put the sidebar on the right to avoid the traffic-light zone.
    #[cfg(target_os = "macos")]
    split_view.set_sidebar_position(gtk::PackType::End);

    // Chart view
    relm4::view! {
        chart_container = gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_vexpand: true,
        }
    }
    let (chart_toolbar, chart_sidebar_btn) = make_tab_toolbar("Chart", &chart_container);
    content_stack.add_named(&chart_toolbar, Some("chart"));

    // Sky view
    relm4::view! {
        sky_container = gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_vexpand: true,
        }
    }
    let (sky_toolbar, sky_sidebar_btn) = make_tab_toolbar("Sky", &sky_container);
    content_stack.add_named(&sky_toolbar, Some("sky"));

    // Network view — full Component owning toolbar, status label, and dynamic body.
    let (network_toolbar, network_tab_sender) = crate::network_tab::launch(
        split_view.clone(),
        sender.input_sender(),
        |out| match out {
            crate::network_tab::NetworkTabOut::ProposeConsent(pid) =>
                AppMsg::ProposeConsent(pid),
        },
    );
    content_stack.add_named(&network_toolbar, Some("network"));

    // Sidebar — Component owning Zodia header, NotifBell pack, nav list, peers slot.
    let (sidebar_toolbar, sidebar_sender) = crate::sidebar::launch(crate::sidebar::SidebarInit {
        peers_list:    peers_list.clone(),
        notif_widget:  notif_widget.clone(),
        split_view:    split_view.clone(),
        content_stack: content_stack.clone(),
    });
    split_view.set_sidebar(Some(&sidebar_toolbar));

    // Peer pages are added dynamically as named children when first opened.

    // Burger button visibility for chart/sky is driven by the collapsed state.
    // (Network tab handles its own sidebar btn internally.)
    // The `collapsed` property itself is driven by an adw::Breakpoint attached
    // to the root window in build_widgets — that is where we have access to
    // the window and can register the breakpoint.
    {
        let btns = [chart_sidebar_btn.clone(), sky_sidebar_btn.clone()];
        split_view.connect_notify_local(Some("collapsed"), move |sv, _| {
            let collapsed = sv.is_collapsed();
            for btn in &btns {
                btn.set_visible(collapsed);
            }
        });
        for btn in [chart_sidebar_btn, sky_sidebar_btn] {
            let sv = split_view.clone();
            btn.connect_clicked(move |_| sv.set_show_sidebar(true));
        }
    }

    // peers_list and notif_widget are now owned by the Sidebar component.
    let _ = (peers_list, notif_widget);

    // ── Outer ToolbarView — just hosts the call bar at bottom ─────────────────

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.set_content(Some(&split_view));

    // Consent request bar — shown when a peer wants to connect.
    relm4::view! {
        consent_bar = gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 10,
            add_css_class: "toolbar",
            set_margin_start: 8,
            set_margin_end: 8,
            set_visible: false,

            #[name(consent_status)]
            gtk::Label {
                set_hexpand: true,
                set_halign: gtk::Align::Start,
            },

            #[name(consent_accept_btn)]
            gtk::Button {
                set_label: "Connect  ✓",
                add_css_class: "suggested-action",
                add_css_class: "pill",
                connect_clicked[sender = sender.clone()] => move |_| {
                    sender.input(AppMsg::AcceptConsent);
                },
            },

            #[name(consent_reject_btn)]
            gtk::Button {
                set_label: "Decline  ✕",
                add_css_class: "destructive-action",
                add_css_class: "pill",
                connect_clicked[sender = sender.clone()] => move |_| {
                    sender.input(AppMsg::RejectConsent);
                },
            },
        }
    }
    toolbar_view.add_bottom_bar(&consent_bar);

    // Call bar — shown during active/ringing/outgoing calls.
    relm4::view! {
        call_bar = gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 10,
            add_css_class: "toolbar",
            set_margin_start: 8,
            set_margin_end: 8,
            set_visible: false,

            #[name(call_status)]
            gtk::Label {
                set_hexpand: true,
                set_halign: gtk::Align::Start,
            },

            #[name(accept_btn)]
            gtk::Button {
                set_label: "Accept  ✓",
                add_css_class: "suggested-action",
                add_css_class: "pill",
                set_visible: false,
                connect_clicked[sender = sender.clone()] => move |_| {
                    sender.input(AppMsg::AcceptCall);
                },
            },

            #[name(hangup_btn)]
            gtk::Button {
                set_label: "Hang up  ✕",
                add_css_class: "destructive-action",
                add_css_class: "pill",
                connect_clicked[sender = sender.clone()] => move |_| {
                    sender.input(AppMsg::HangUp);
                },
            },
        }
    }
    toolbar_view.add_bottom_bar(&call_bar);

    let _ = model;

    (
        toolbar_view,
        chart_container, sky_container,
        split_view,
        content_stack,
        network_tab_sender,
        sidebar_sender,
        consent_bar, consent_status, consent_accept_btn, consent_reject_btn,
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

/// Load Connected peers from `peers.tsv` → `{peer_id_hex64}\t{jdn}\t{geohash}`.
fn load_connected_stargazers(data_dir: &std::path::Path) -> HashMap<PeerId, zodia_net::ConsentBlob> {
    let Ok(content) = std::fs::read_to_string(data_dir.join("peers.tsv")) else {
        return HashMap::new();
    };
    content.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let id_hex   = parts.next()?;
            let jdn: f64 = parts.next()?.parse().ok()?;
            let geohash  = parts.next()?.to_string();
            let id_arr: [u8; 32] = hex::decode(id_hex).ok()?.try_into().ok()?;
            let blob = zodia_net::ConsentBlob {
                birth:    zodia_core::BirthData::new(jdn, geohash),
                prekey:   [0u8; 32],
                ephemeral:[0u8; 32],
                relay_pk: [0u8; 32],
            };
            Some((PeerId(id_arr), blob))
        })
        .collect()
}

fn save_stargazers(data_dir: &std::path::Path, stargazers: &HashMap<PeerId, Stargazer>) {
    let content: String = stargazers.values()
        .filter_map(|s| match &s.state {
            StargazerState::Connected { birth } =>
                Some(format!("{}\t{}\t{}\n", hex::encode(&s.peer_id.0), birth.birth.jdn, birth.birth.geohash)),
            _ => None,
        })
        .collect();
    let _ = std::fs::write(data_dir.join("peers.tsv"), content);
}

/// Load OutgoingPending peers from `pending.tsv` (one hex64 peer id per line).
fn load_pending(data_dir: &std::path::Path) -> Vec<PeerId> {
    let Ok(content) = std::fs::read_to_string(data_dir.join("pending.tsv")) else {
        return Vec::new();
    };
    content.lines()
        .filter_map(|line| {
            let arr: [u8; 32] = hex::decode(line.trim()).ok()?.try_into().ok()?;
            Some(PeerId(arr))
        })
        .collect()
}

fn save_pending(data_dir: &std::path::Path, stargazers: &HashMap<PeerId, Stargazer>) {
    let content: String = stargazers.values()
        .filter(|s| matches!(s.state, StargazerState::OutgoingPending))
        .map(|s| format!("{}\n", hex::encode(&s.peer_id.0)))
        .collect();
    let _ = std::fs::write(data_dir.join("pending.tsv"), content);
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
