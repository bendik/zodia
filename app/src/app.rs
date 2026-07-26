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
use zodia_core::{birth_from_coords, compute_synastry, gregorian_to_jdn,
                 Chart, InterpKey};
use zodia_crypto::IdentityKeypair;
use zodia_crypto::{ecies_decrypt, ecies_encrypt};
use zodia_net::{ChannelMsg, ConsentBlob, DirectChannel, InterpEntry,
                NetworkConfig, PeerId, PeerStatus, RelayPayload, ZodiaNetEvent, ZodiaNetwork};
use zodia_store::{StoreError, ZodiaStore, BaselineStore};
use zodia_pipeline::StateEvent;
use zodia_sdk::{SyncLifecycleEvent, ZodiaClient};

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

// ── incoming-channel routing ──────────────────────────────────────────────────

/// Why we're auto-accepting an incoming consent exchange without showing
/// the user a prompt.  Determines whether `ConnectionComplete` runs the
/// first-time-connect side-effects (notification, persistence write).
#[derive(Debug, Clone, Copy)]
enum AutoAcceptKind {
    /// We're seeking them too — first-time mutual consent.
    FirstTime,
    /// They were Connected previously and are reconnecting; suppress notify.
    Reconnect,
}

// ── sync status ───────────────────────────────────────────────────────────────

/// Tagged sync lifecycle event for AppMsg routing.
#[derive(Debug, Clone)]
pub enum SyncLifecycle {
    Started  { remote_pk: [u8; 32] },
    Finished { remote_pk: [u8; 32], received_ops: u64 },
    Failed   { remote_pk: [u8; 32], error: String },
}

/// Per-peer sync state shown in the Network tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPeerStatus {
    Syncing,
    CaughtUp { received_ops: u64 },
    Failed   { error: String },
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
    /// A typed state event from the inbound `ZodiaPipeline`.  Replaces
    /// the legacy `SyncInterpReceived` path: now everything that arrives
    /// over LogSync flows through the pipeline first.
    SyncStateEvent(StateEvent),
    /// A LogSync session lifecycle event (started / finished / failed)
    /// for surfacing in the Network tab.
    SyncLifecycle(SyncLifecycle),
    /// User asked to revoke + delete a self-authored interpretation.
    /// Authorisation enforced at the store: only rows whose `author_pk`
    /// matches the local identity are tombstoned.  Network propagation
    /// via `InterpOp::Revoke` so peers honour the tombstone as well.
    SubmitRevoke { log_id: [u8; 32] },
    /// Phase F-collab: user submitted an edit on the collaborative
    /// community doc for `interp_key`.  Applies locally, persists snapshot,
    /// broadcasts as `DocOp::Edit`.
    PublishDocEdit { interp_key: String, new_body: String },
    /// Phase F-collab: user affirmed the current revision of a doc.
    AffirmDocRev { interp_key: String, target_rev: [u8; 32] },
    /// Phase F-collab: user proposed a veto on the newest edit of a doc.
    /// Authority gate (ring + window + newest-edit) checked locally before
    /// publish; if it passes, local rollback runs and `DocOp::Veto` ships.
    ProposeDocVeto { interp_key: String, target_edit_op_id: [u8; 32] },
    /// Phase F-collab: user joined / left an editor session.  Heartbeat.
    EditorPresence { interp_key: String, joined: bool },
    /// An aspect page for `interp_key` opened. If the key isn't in the
    /// user's own chart (already permanently subscribed at startup — see
    /// `subscribe_own_chart_keys`), opens/extends its lazy, grace-period
    /// subscription (docs/prd/granular-topic-subscription.md). No-op for
    /// chart keys and re-opens both — `touch_subscription` is idempotent
    /// and resetting the grace clock on every visit is the intended
    /// behavior, not a bug.
    TouchKeySubscription { interp_key: String },
    /// Phase F-collab: start voice mesh with every peer currently present
    /// in `interp_key`'s editor session that we have a `DirectChannel` to.
    /// Reuses the legacy `CallStargazer` machinery per pair — small mesh,
    /// no SFU.
    StartEditorAudio { interp_key: String },
    /// Phase E: user explicitly toggled read-state on a feed card via the
    /// context menu.  `read = true` inserts into `feed_read`; `false` removes.
    /// Bell badge refreshes after either branch.
    FeedSetRead { event_id: [u8; 32], read: bool },
    /// Phase E: a `FeedItem` was activated (clicked) in the Sky / per-aspect
    /// FeedView.  Marks the event as read in the store and (where relevant)
    /// navigates to the linked content.
    FeedActivated {
        event_id: [u8; 32],
        payload:  crate::feed_view::ActivatedPayload,
    },
    /// Phase E: re-pull the bell badge count from the store (after persisting
    /// a new targeting event or marking events read).
    RefreshBellBadge,
    /// Phase E: user clicked the notification bell.  Bulk-marks all currently
    /// pending targeting events as read and navigates to Sky.
    BellClicked,
    /// Phase E internal: store the freshly-spawned `FeedView` sender on the
    /// model.  Emitted by `update_view` after the lazy Sky-feed mount.
    #[doc(hidden)]
    __SetFeedSender(relm4::Sender<crate::feed_view::FeedViewMsg>),
    /// Phase E internal: deliver an initial batch of feed items (synthesised
    /// from the store) into the live FeedView.
    #[doc(hidden)]
    __BackfillFeed(Vec<crate::feed_item::FeedItem>),
    /// Phase E internal: push a single live feed item into the FeedView.
    #[doc(hidden)]
    __PushFeedItem(crate::feed_item::FeedItem),
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
    zodia_client: Option<ZodiaClient>,

    /// Most recent community interpretation contributions, for the network tab.
    recent_interps: Vec<zodia_store::RecentInterp>,

    /// Per-remote-pubkey LogSync session status, surfaced in the network tab.
    /// Keyed by the p2panda `VerifyingKey` bytes of the remote peer.
    sync_peer_status: HashMap<[u8; 32], SyncPeerStatus>,

    /// Sender for the Sky-tab FeedView component.  Set once the Sky pane is
    /// populated (lazily, after chart is available).  `SyncStateEvent`
    /// handlers convert pipeline events into `FeedItem`s and push here so
    /// the live feed updates without a full Sky refresh.
    feed_view_sender: Option<relm4::Sender<crate::feed_view::FeedViewMsg>>,

    /// Count of unread events targeting the local identity (affirmations on
    /// my interps, responses to my threads).  Refreshed from the store
    /// whenever a relevant event arrives or `feed_read` mutates.
    feed_targeting_unread: u64,

    /// Handle to the running transit-ticker supervisor.  Aborted + replaced
    /// when the local chart changes (e.g. user re-runs setup).  Inner ticker
    /// task exits within `TICK_INTERVAL` of the supervisor drop.
    transit_ticker_handle: Option<tokio::task::JoinHandle<()>>,

    /// Phase E: feed items that arrived before `feed_view_sender` was wired.
    /// Drained on `__SetFeedSender`.  Fixes a race where the transit ticker
    /// (spawned at chart load) emits its first batch before the lazy Sky-tab
    /// mount finishes.
    feed_pending: Vec<crate::feed_item::FeedItem>,

    /// Sky-tab `NavigationView` host.  Used by `FeedActivated` to push the
    /// community-interpretations detail page for the clicked card's key.
    /// `Rc<RefCell<_>>` because `NavigationView` is `!Send` and can't travel
    /// through the message bus.
    sky_nav: Rc<RefCell<Option<adw::NavigationView>>>,

    /// Bell-click navigation token.  Bumped on `BellClicked`; `update_view`
    /// compares against its shown counter and, when they differ, switches
    /// the content stack to the Sky tab.  Sky is the bell's destination per
    /// the activity-feed PRD (the bell is the "stuff about you" bulk-ack
    /// surface and lives next to the feed).
    nav_to_sky_token: u64,

    /// Phase F-collab: who is currently present in each key's editor
    /// session (excluding the local user), with the last-seen unix
    /// timestamp per peer.  Entries older than `PRESENCE_TTL_SECS` get
    /// pruned on read.  Refreshed on each `StateEvent::EditorPresenceChanged`
    /// with `joined=true`; `joined=false` removes immediately.
    editor_presence: HashMap<String, HashMap<[u8; 32], u64>>,
}

/// Client-side time-to-live for a remote peer's "I'm editing" heartbeat.
/// If we haven't heard a fresh `joined=true` op from them within this
/// window, we drop them from `editor_presence` so audio mesh + presence
/// chips don't show stale peers after a crash or network partition.
const PRESENCE_TTL_SECS: u64 = 5 * 60;

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

    /// Phase F-collab: sidebar Discussions list (one row per interp_key with
    /// active editor presence).  Rebuilt on every `update_view` from
    /// `model.editor_presence`.
    discussions_list:   gtk::ListBox,
    discussions_header: gtk::Label,

    /// Counter of the last bell-click we acted on.  Diverges from
    /// `AppModel::nav_to_sky_token` only when a new click is pending; switch
    /// happens in `update_view` and the counter is bumped to match.
    nav_to_sky_token_shown: u64,

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
            zodia_client: None,
            recent_interps: Vec::new(),
            sync_peer_status: HashMap::new(),
            feed_view_sender: None,
            feed_targeting_unread: 0,
            transit_ticker_handle: None,
            feed_pending: Vec::new(),
            sky_nav: Rc::new(RefCell::new(None)),
            nav_to_sky_token: 0,
            editor_presence: HashMap::new(),
        };
        // Populate the factory with persisted peers (Connected + OutgoingPending).
        sync_peers_factory(&mut model);
        model.recent_interps = model.store
            .recent_community_interps(12).await.unwrap_or_default();

        // Phase F-collab one-time migration: fold legacy `interpretations`
        // rows into per-key collab docs.  Idempotent guard via feed_meta.
        if let Err(e) = migrate_interps_to_docs(
            &model.store, &model.baseline, &model.identity.public_key(),
        ).await {
            warn!("collab-doc migration failed: {e}");
        }

        if let Some(birth) = model.config.birth.clone() {
            if let Ok(chart) = Chart::compute(birth.clone()) {
                model.transit_ticker_handle = Some(spawn_transit_ticker(
                    chart.clone(),
                    Rc::clone(&model.baseline),
                    model.store.clone(),
                    sender.clone(),
                ));
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
        let bell_sender = sender.input_sender().clone();
        let (notif_widget, notif_sender) = crate::notif_bell::launch(move || {
            let _ = bell_sender.send(AppMsg::BellClicked);
        });
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
                model.zodia_client = try_spawn_sync(&model.config, &net, &sender).await;
                if let (Some(client), Some(chart)) = (&model.zodia_client, &model.chart) {
                    subscribe_own_chart_keys(chart, client).await;
                }
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
                    Ok(chart) => {
                        // Abort any prior ticker before swapping chart.
                        if let Some(h) = self.transit_ticker_handle.take() {
                            h.abort();
                        }
                        self.transit_ticker_handle = Some(spawn_transit_ticker(
                            chart.clone(),
                            Rc::clone(&self.baseline),
                            self.store.clone(),
                            sender.clone(),
                        ));
                        self.chart = Some(chart);
                    }
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
                    self.zodia_client = try_spawn_sync(&self.config, &net, &sender).await;
                    if let (Some(client), Some(chart)) = (&self.zodia_client, &self.chart) {
                        subscribe_own_chart_keys(chart, client).await;
                    }
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
            AppMsg::SyncStateEvent(event) => {
                let me: [u8; 32] = self.identity.public_key();
                let now_ts: u64 = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mut feed_item: Option<crate::feed_item::FeedItem> = None;
                match &event {
                    StateEvent::InterpAuthored { author, interp_key, body, .. } => {
                        let author_pk: [u8; 32] = *author.as_bytes();
                        match self.store
                            .insert_from_op(interp_key, body, &author_pk)
                            .await
                        {
                            Ok(true) => {
                                debug!(
                                    key    = %interp_key,
                                    author = %hex::encode(&author_pk[..4]),
                                    "interp authored via sync — stored"
                                );
                                self.recent_interps = self.store
                                    .recent_community_interps(12).await.unwrap_or_default();
                                self.network_changed_token += 1;
                                // Author events never "target" the local user
                                // (they ARE the local user when authored
                                // locally; for remote ones they're ambient).
                                feed_item = crate::feed_item::state_event_to_feed_item(
                                    &event, now_ts, interp_key.clone(), false,
                                );
                            }
                            Ok(false) => {} // duplicate, nothing to do
                            Err(e) => warn!("sync insert_from_op failed: {e}"),
                        }
                    }
                    StateEvent::AffirmAdded { target_log_id, voter } => {
                        let log_id: [u8; 32] = *target_log_id.as_bytes();
                        let voter_pk: [u8; 32] = *voter.as_bytes();
                        match self.store.affirm(&log_id, &voter_pk).await {
                            Ok(true) => {
                                debug!(
                                    voter = %hex::encode(&voter_pk[..4]),
                                    "remote affirmation persisted"
                                );
                                self.network_changed_token += 1;
                                // Look up the target's interp_key + author.
                                let (target_key, targets_me) =
                                    affirm_lookup(&self.store, &log_id, &voter_pk, &me).await;
                                feed_item = crate::feed_item::state_event_to_feed_item(
                                    &event, now_ts, target_key, targets_me,
                                );
                            }
                            Ok(false) => {} // already had this affirm
                            Err(e) => warn!("sync affirm persist failed: {e}"),
                        }
                    }
                    StateEvent::ResponseAdded { parent_log_id, author, body: _, .. } => {
                        let parent: [u8; 32] = *parent_log_id.as_bytes();
                        let author_pk: [u8; 32] = *author.as_bytes();
                        // capture body for store call
                        let body_str = if let StateEvent::ResponseAdded { body, .. } = &event {
                            body.clone()
                        } else { String::new() };
                        match self.store
                            .insert_response_from_op(&parent, &body_str, &author_pk)
                            .await
                        {
                            Ok(true) => {
                                debug!(
                                    author = %hex::encode(&author_pk[..4]),
                                    parent = %hex::encode(&parent[..4]),
                                    "remote response persisted"
                                );
                                self.network_changed_token += 1;
                                let (parent_key, targets_me) =
                                    response_parent_lookup(
                                        &self.store, &parent, &author_pk, &me,
                                    ).await;
                                feed_item = crate::feed_item::state_event_to_feed_item(
                                    &event, now_ts, parent_key, targets_me,
                                );
                            }
                            Ok(false) => {} // already had this response
                            Err(e) => warn!("sync response persist failed: {e}"),
                        }
                    }
                    StateEvent::Skipped { reason } => {
                        debug!(?reason, "sync op skipped");
                    }
                    StateEvent::DocEdited {
                        interp_key, by, crdt_update, affected_blocks, timestamp, ..
                    } => {
                        let me: [u8; 32] = self.identity.public_key();
                        let pk_vec = by.as_bytes().to_vec();
                        let editor_pk: [u8; 32] = *by.as_bytes();
                        // Pre-check ring membership BEFORE the edit lands —
                        // after `block_ring_push` the editor sits at newest,
                        // potentially evicting the old "you" entry.
                        let mut you_were_in_ring = false;
                        if editor_pk != me {
                            for block_id in affected_blocks {
                                let entries = self.store
                                    .block_ring_get(interp_key, block_id)
                                    .await.unwrap_or_default();
                                if entries.iter().any(|(a, _, _)| a == &me) {
                                    you_were_in_ring = true;
                                    break;
                                }
                            }
                        }
                        if let Err(e) = apply_doc_edit(
                            &self.store, interp_key, crdt_update, &pk_vec,
                            &op_id_for(&event), *timestamp, affected_blocks, &me,
                        ).await {
                            warn!("doc edit apply failed: {e}");
                        }
                        push_doc_body_refresh(&self.store, interp_key, &me).await;
                        feed_item = crate::feed_item::state_event_to_feed_item(
                            &event, now_ts, interp_key.clone(), false,
                        );
                        // Bell-targeting follow-up card: your block was just
                        // edited — surface a high-salience prompt to review
                        // + veto within the window.
                        if you_were_in_ring {
                            if let Some(s) = &self.feed_view_sender {
                                let item = crate::feed_item::FeedItem::block_you_authored_was_edited(
                                    interp_key, &editor_pk, &op_id_for(&event), now_ts,
                                );
                                let _ = s.send(crate::feed_view::FeedViewMsg::Push(item));
                            }
                            // Bump bell badge directly — synthetic events
                            // aren't in `feed_read`-backed op tables yet, so
                            // RefreshBellBadge alone wouldn't catch them.
                            self.feed_targeting_unread += 1;
                            self.network_changed_token += 1;
                        }
                    }
                    StateEvent::DocVetoProposed {
                        interp_key, target_edit_op_id, by, ..
                    } => {
                        let revoker: [u8; 32] = *by.as_bytes();
                        let target = *target_edit_op_id.as_bytes();
                        if let Some((key, author, edit_op)) = apply_doc_veto(
                            &self.store, &target, &revoker, Some(interp_key), now_ts,
                        ).await {
                            self.network_changed_token += 1;
                            push_doc_body_refresh(&self.store, &key, &self.identity.public_key()).await;
                            if let Some(s) = &self.feed_view_sender {
                                let item = crate::feed_item::FeedItem::doc_rolled_back(
                                    &key, &author, &revoker, &edit_op, now_ts,
                                );
                                let _ = s.send(
                                    crate::feed_view::FeedViewMsg::Push(item),
                                );
                            }
                        }
                    }
                    StateEvent::DocAffirmed { interp_key, target_rev, by, .. } => {
                        let voter = by.as_bytes();
                        let _ = self.store.doc_affirm_rev(
                            interp_key, target_rev, voter,
                        ).await;
                        feed_item = crate::feed_item::state_event_to_feed_item(
                            &event, now_ts, interp_key.clone(), false,
                        );
                    }
                    StateEvent::EditorPresenceChanged { interp_key, by, joined, timestamp, .. } => {
                        let me: [u8; 32] = self.identity.public_key();
                        let peer_pk: [u8; 32] = *by.as_bytes();
                        // Don't track ourselves in remote-presence.
                        if peer_pk != me {
                            let entry = self.editor_presence
                                .entry(interp_key.clone())
                                .or_default();
                            if *joined {
                                entry.insert(peer_pk, *timestamp);
                            } else {
                                entry.remove(&peer_pk);
                            }
                            self.network_changed_token += 1;
                        }
                        feed_item = crate::feed_item::state_event_to_feed_item(
                            &event, now_ts, interp_key.clone(), false,
                        );
                    }
                    StateEvent::InterpRevoked { target_log_id, by, .. } => {
                        let log_id: [u8; 32] = *target_log_id.as_bytes();
                        let by_pk:  [u8; 32] = *by.as_bytes();
                        // Authorization: only honour if `by` matches the
                        // original row's author.  `revoke_interp` enforces
                        // the constraint atomically in SQL.
                        match self.store.revoke_interp(&log_id, &by_pk).await {
                            Ok(true) => {
                                debug!(
                                    by = %hex::encode(&by_pk[..4]),
                                    log = %hex::encode(&log_id[..4]),
                                    "interpretation revoked"
                                );
                                self.network_changed_token += 1;
                                // Refresh feed from store: easiest way to
                                // drop the now-tombstoned card.
                                if let Some(s) = &self.feed_view_sender {
                                    let me: [u8; 32] = self.identity.public_key();
                                    let rows = self.store.recent_feed_rows(&me, 200)
                                        .await.unwrap_or_default();
                                    let items: Vec<crate::feed_item::FeedItem> = rows.iter()
                                        .map(|r| crate::feed_item::feed_row_to_feed_item(r, false))
                                        .collect();
                                    let _ = s.send(crate::feed_view::FeedViewMsg::Reset(items));
                                }
                            }
                            Ok(false) => {
                                debug!("ignored revoke: author mismatch or already revoked");
                            }
                            Err(e) => warn!("revoke persist failed: {e}"),
                        }
                    }
                }
                if let Some(item) = feed_item {
                    if let Some(s) = &self.feed_view_sender {
                        let _ = s.send(crate::feed_view::FeedViewMsg::Push(item));
                    }
                    // A new targeting event may need to badge the bell.
                    let _ = sender.input_sender().send(AppMsg::RefreshBellBadge);
                }
            }
            AppMsg::SyncLifecycle(lifecycle) => {
                let (remote_pk, status) = match lifecycle {
                    SyncLifecycle::Started { remote_pk } => {
                        (remote_pk, SyncPeerStatus::Syncing)
                    }
                    SyncLifecycle::Finished { remote_pk, received_ops } => {
                        (remote_pk, SyncPeerStatus::CaughtUp { received_ops })
                    }
                    SyncLifecycle::Failed { remote_pk, error } => {
                        (remote_pk, SyncPeerStatus::Failed { error })
                    }
                };
                debug!(
                    peer = %hex::encode_upper(&remote_pk[..4]),
                    ?status,
                    "sync lifecycle"
                );
                self.sync_peer_status.insert(remote_pk, status);
                self.network_changed_token += 1;
            }
            AppMsg::PublishDocEdit { interp_key, new_body } => {
                let me: [u8; 32] = self.identity.public_key();
                let me_vk = match zodia_doc::VerifyingKey::from_bytes(&me) {
                    Ok(vk) => vk,
                    Err(_) => return,
                };
                // Load or create local doc, capture prior bytes, apply body.
                let prior_bytes = match self.store.doc_load(&interp_key).await {
                    Ok(b) => b,
                    Err(e) => { warn!("doc_load: {e}"); return; }
                };
                let doc = match prior_bytes.as_deref() {
                    Some(bytes) => zodia_doc::InterpDoc::from_snapshot(&me_vk, bytes)
                        .unwrap_or_else(|_| zodia_doc::InterpDoc::empty(&me_vk)),
                    None => zodia_doc::InterpDoc::empty(&me_vk),
                };
                let base_rev = doc.current_rev();
                if let Err(e) = doc.set_body(&new_body) {
                    warn!("doc set_body: {e}"); return;
                }
                let edit = match doc.publish_local() {
                    Ok(e)  => e,
                    Err(e) => { warn!("doc publish_local: {e}"); return; }
                };
                // Derive a local edit op_id deterministically from the local
                // identity + base_rev + the update bytes.  The wire op gets a
                // p2panda header hash on send; for the local rollback table
                // we use this synthetic id so the same row can be matched
                // against the eventual incoming `DocVetoProposed` if a peer
                // vetoes (the remote veto carries the wire op_id which won't
                // equal this synthetic id — out of scope for tracer; multi-
                // device sync fills this gap in Phase J).
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"local_edit");
                hasher.update(&me);
                hasher.update(&base_rev);
                hasher.update(&edit.update_bytes);
                let edit_op_id = *hasher.finalize().as_bytes();
                let blocks_cbor = encode_blocks(&edit.affected_blocks);
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs()).unwrap_or(0);
                if let Ok(snap) = doc.snapshot() {
                    let _ = self.store.doc_save_with_history(
                        &interp_key, &snap, &edit.rev,
                        prior_bytes.as_deref(),
                        &edit_op_id, ts, &me, &blocks_cbor,
                    ).await;
                }
                // Broadcast via sync.
                if let Some(client) = &self.zodia_client {
                    let base_rev = p2panda_core::Hash::from_bytes(base_rev);
                    if let Err(e) = client.edit(
                        &interp_key, base_rev, edit.update_bytes, edit.affected_blocks,
                    ).await {
                        warn!("edit publish: {e}");
                    }
                }
                self.network_changed_token += 1;
            }
            AppMsg::ProposeDocVeto { interp_key, target_edit_op_id } => {
                let me: [u8; 32] = self.identity.public_key();
                let now_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs()).unwrap_or(0);
                if let Some((key, author, edit_op)) = apply_doc_veto(
                    &self.store, &target_edit_op_id, &me, Some(&interp_key), now_ts,
                ).await {
                    self.network_changed_token += 1;
                    push_doc_body_refresh(&self.store, &key, &me).await;
                    if let Some(client) = &self.zodia_client {
                        let target = p2panda_core::Hash::from_bytes(target_edit_op_id);
                        if let Err(e) = client.veto(&key, target).await {
                            warn!("veto publish: {e}");
                        }
                    }
                    if let Some(s) = &self.feed_view_sender {
                        let item = crate::feed_item::FeedItem::doc_rolled_back(
                            &key, &author, &me, &edit_op, now_ts,
                        );
                        let _ = s.send(crate::feed_view::FeedViewMsg::Push(item));
                    }
                } else {
                    info!(key = %interp_key, "veto not authorised (ring/window/stale)");
                }
            }
            AppMsg::AffirmDocRev { interp_key, target_rev } => {
                let me: [u8; 32] = self.identity.public_key();
                let _ = self.store.doc_affirm_rev(&interp_key, &target_rev, &me).await;
                if let Some(client) = &self.zodia_client {
                    if let Err(e) = client.affirm_rev(&interp_key, target_rev).await {
                        warn!("affirm_rev publish: {e}");
                    }
                }
            }
            AppMsg::EditorPresence { interp_key, joined } => {
                if let Some(client) = &self.zodia_client {
                    if let Err(e) = client.set_editor_presence(&interp_key, joined).await {
                        warn!("editor presence publish: {e}");
                    }
                }
            }
            AppMsg::TouchKeySubscription { interp_key } => {
                if let (Some(client), Some(chart)) = (&self.zodia_client, &self.chart) {
                    if needs_lazy_subscription(&interp_key, chart) {
                        // Matches transit_ticker.rs's TICK_INTERVAL — the
                        // app's existing background-refresh cadence, reused
                        // here rather than inventing a second constant.
                        const GRACE: std::time::Duration = std::time::Duration::from_secs(600);
                        if let Err(e) = client.touch_subscription(&interp_key, GRACE).await {
                            warn!("touch_subscription {interp_key}: {e}");
                        }
                    }
                }
            }
            AppMsg::StartEditorAudio { interp_key } => {
                // Prune stale presence first, then collect fresh peers.
                prune_stale_presence(&mut self.editor_presence);
                let present: Vec<[u8; 32]> = self.editor_presence
                    .get(&interp_key)
                    .map(|m| m.keys().copied().collect())
                    .unwrap_or_default();
                if present.is_empty() {
                    info!(key = %interp_key, "no other peers present — audio noop");
                    return;
                }
                // Walk present peers; for each that we have a Connected
                // DirectChannel to, dispatch CallStargazer to reuse the
                // existing 1:1 AudioSession start flow.  Mesh = N pair
                // sessions running concurrently.  Hard-cap at 6 per the
                // earlier hang-design grilling.
                let cap = 6usize;
                let mut started = 0;
                for peer_pk in present.iter() {
                    if started >= cap { break; }
                    for (pid, s) in self.stargazers.iter() {
                        if &pid.0 == peer_pk {
                            if matches!(s.state, StargazerState::Connected { .. }) {
                                let _ = sender.input_sender().send(
                                    AppMsg::CallStargazer(pid.clone())
                                );
                                started += 1;
                            }
                            break;
                        }
                    }
                }
                if started == 0 {
                    info!(
                        key = %interp_key,
                        n_present = present.len(),
                        "audio: no Connected peers in editor session"
                    );
                }
            }
            AppMsg::SubmitRevoke { log_id } => {
                let me: [u8; 32] = self.identity.public_key();
                // Local tombstone first — authorisation enforced in SQL.
                match self.store.revoke_interp(&log_id, &me).await {
                    Ok(true) => {
                        info!(
                            log = %hex::encode(&log_id[..4]),
                            "revoked own interpretation"
                        );
                        self.network_changed_token += 1;
                        // Refresh feed.
                        if let Some(s) = &self.feed_view_sender {
                            let rows = self.store.recent_feed_rows(&me, 200)
                                .await.unwrap_or_default();
                            let items: Vec<crate::feed_item::FeedItem> = rows.iter()
                                .map(|r| crate::feed_item::feed_row_to_feed_item(r, false))
                                .collect();
                            let _ = s.send(crate::feed_view::FeedViewMsg::Reset(items));
                        }
                        // Propagate to peers.
                        if let Some(client) = &self.zodia_client {
                            let target = p2panda_core::Hash::from_bytes(log_id);
                            if let Err(e) = client.revoke(target).await {
                                warn!("revoke publish: {e}");
                            }
                        }
                    }
                    Ok(false) => warn!("revoke ignored: not your row, or already revoked"),
                    Err(e)    => warn!("revoke failed: {e}"),
                }
            }
            AppMsg::__SetFeedSender(s) => {
                // Drain any items that arrived before the FeedView mounted.
                let pending = std::mem::take(&mut self.feed_pending);
                for item in pending {
                    let _ = s.send(crate::feed_view::FeedViewMsg::Push(item));
                }
                self.feed_view_sender = Some(s);
            }
            AppMsg::__BackfillFeed(items) => {
                if let Some(s) = &self.feed_view_sender {
                    // Per-item Push (not Reset) so live items pushed before
                    // backfill resolved aren't wiped out.  Dedup is handled
                    // by FeedView via event_id.
                    for item in items {
                        let _ = s.send(crate::feed_view::FeedViewMsg::Push(item));
                    }
                } else {
                    self.feed_pending.extend(items);
                }
            }
            AppMsg::__PushFeedItem(item) => {
                if let Some(s) = &self.feed_view_sender {
                    let _ = s.send(crate::feed_view::FeedViewMsg::Push(item));
                } else {
                    self.feed_pending.push(item);
                }
            }
            AppMsg::FeedActivated { event_id, payload } => {
                // Persist read-state.
                if let Err(e) = self.store.mark_event_read(&event_id).await {
                    warn!("feed mark_event_read: {e}");
                }
                let _ = sender.input_sender().send(AppMsg::RefreshBellBadge);
                if let Some(s) = &self.feed_view_sender {
                    let _ = s.send(crate::feed_view::FeedViewMsg::MarkRead(event_id));
                }

                // Navigation: parse the canonical key string back into an
                // `InterpKey` and push the existing aspect-detail page onto
                // Sky's NavigationView.  Sky-aspect keys (`sky:…`) and
                // anything without a parseable key fall through silently.
                use crate::feed_view::ActivatedPayload;
                let key_str: Option<String> = match payload {
                    ActivatedPayload::OpenInterpKey(k)              => Some(k),
                    ActivatedPayload::OpenAffirmTarget { target_key, .. } => Some(target_key),
                    ActivatedPayload::OpenResponseParent { interp_key, .. } => Some(interp_key),
                    ActivatedPayload::OpenTransitKey(k)             => Some(k),
                };
                let Some(key_str) = key_str else { return; };
                let Some(parsed) = zodia_core::parse_interp_sig(&key_str) else {
                    debug!(key = %key_str, "feed: no parser for key — skipping nav");
                    return;
                };
                let keys = vec![crate::aspect_list::KeyEntry {
                    label: "Aspect".to_string(),
                    key:   parsed,
                }];
                let page = aspect_view::detail_page(
                    &keys,
                    None,
                    &self.store,
                    &self.baseline,
                    Rc::clone(&self.identity),
                    sender.clone(),
                ).await;
                if let Some(nav) = self.sky_nav.borrow().as_ref() {
                    nav.push(&page);
                }
            }
            AppMsg::RefreshBellBadge => {
                let me: [u8; 32] = self.identity.public_key();
                match self.store.feed_targeting_unread_count(&me).await {
                    Ok(n)  => self.feed_targeting_unread = n,
                    Err(e) => warn!("feed_targeting_unread_count: {e}"),
                }
            }
            AppMsg::FeedSetRead { event_id, read } => {
                let res = if read {
                    self.store.mark_event_read(&event_id).await
                } else {
                    self.store.mark_event_unread(&event_id).await
                };
                if let Err(e) = res { warn!("feed_set_read: {e}"); }
                let _ = sender.input_sender().send(AppMsg::RefreshBellBadge);
            }
            AppMsg::BellClicked => {
                self.nav_to_sky_token = self.nav_to_sky_token.wrapping_add(1);
                let me: [u8; 32] = self.identity.public_key();
                match self.store.feed_targeting_unread_ids(&me).await {
                    Ok(ids) => {
                        if let Err(e) = self.store.bulk_mark_read(&ids).await {
                            warn!("bell bulk_mark_read: {e}");
                        }
                        for id in &ids {
                            if let Some(s) = &self.feed_view_sender {
                                let _ = s.send(crate::feed_view::FeedViewMsg::MarkRead(*id));
                            }
                        }
                    }
                    Err(e) => warn!("bell unread_ids: {e}"),
                }
                let _ = sender.input_sender().send(AppMsg::RefreshBellBadge);
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

                // Auto-accept fast paths — skip the consent bar when we
                // already trust this peer:
                //   * `OutgoingPending`     — we're seeking them too (mutual
                //                             pending, classic happy path)
                //   * `Connected { .. }`    — we've consented before, this is
                //                             just a reconnection after one
                //                             side restarted or roamed
                let auto_kind = match self.stargazers.get(&peer_id).map(|s| &s.state) {
                    Some(StargazerState::OutgoingPending)      => Some(AutoAcceptKind::FirstTime),
                    Some(StargazerState::Connected { .. })     => Some(AutoAcceptKind::Reconnect),
                    _                                          => None,
                };
                if let Some(kind) = auto_kind {
                    let reason = match kind {
                        AutoAcceptKind::FirstTime => "mutual pending",
                        AutoAcceptKind::Reconnect => "already-connected peer reconnecting",
                    };
                    info!(peer = %peer_hex, reason, "auto-accepting incoming channel");
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
                                        is_new: matches!(kind, AutoAcceptKind::FirstTime),
                                    });
                                }
                                Err(e) => warn!(peer = %peer_hex, "auto-accept failed: {e}"),
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

                // Phase E: Sky becomes the activity feed.  Mount the FeedView
                // here on first paint (no transit aspect-table any more).
                let (feed_sender, nav) = mount_sky_feed(
                    &widgets.sky_container,
                    &sender,
                    self.identity.public_key(),
                );
                *self.sky_nav.borrow_mut() = Some(nav);
                // Stash the sender for live-event forwarding from SyncStateEvent.
                // Interior mutability via the parent_sender → message round-trip
                // would be cleaner but adds a frame of latency; assigning via
                // direct field access on `&self` isn't possible inside
                // `update_view (&self)`, so we route through a message.
                let _ = sender.input_sender().send(
                    AppMsg::__SetFeedSender(feed_sender)
                );
                // Kick off a backfill from the store.
                let me: [u8; 32] = self.identity.public_key();
                let store = self.store.clone();
                let s2 = sender.clone();
                relm4::spawn_local(async move {
                    if let Ok(rows) = store.recent_feed_rows(&me, 200).await {
                        let items: Vec<crate::feed_item::FeedItem> = rows.iter()
                            .map(|r| crate::feed_item::feed_row_to_feed_item(r, false))
                            .collect();
                        let _ = s2.input_sender().send(AppMsg::__BackfillFeed(items));
                    }
                });
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
        //
        // Two sources contribute to the bell: per-peer chat unread (kept from
        // pre-Phase E) and Phase E's feed-targeting unread (affirmations on
        // your interps, responses to your threads).  Both flow into the same
        // popover summary and badge count.

        if let Some(s) = &self.notif_sender {
            let chat_total: usize = self.unread_messages.values().sum();
            let feed_total: usize = self.feed_targeting_unread as usize;
            let total_unread = chat_total + feed_total;
            let chat_summary: String = self.unread_messages.iter()
                .filter(|(_, &n)| n > 0)
                .map(|(tag, n)| {
                    let name = self.stargazer_nicknames.get(tag)
                        .cloned()
                        .unwrap_or_else(|| format!("···{tag}"));
                    format!("{name}  ·  {n} unread")
                })
                .collect::<Vec<_>>()
                .join("\n");
            let summary = match (feed_total, chat_summary.is_empty()) {
                (0, true)  => String::new(),
                (0, false) => chat_summary,
                (n, true)  => format!("{n} new in feed"),
                (n, false) => format!("{n} new in feed\n{chat_summary}"),
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

        // ── Bell-click → Sky tab nav ──────────────────────────────────────────
        if widgets.nav_to_sky_token_shown != self.nav_to_sky_token {
            widgets.nav_to_sky_token_shown = self.nav_to_sky_token;
            widgets.content_stack.set_visible_child_name("sky");
        }

        // ── Phase F-collab: rebuild sidebar Discussions list ──────────────────
        // One row per interp_key with active editor presence (peers + self).
        // Currently the model only tracks *other peers* in editor_presence;
        // self-presence is implicit (you joined when you opened the page).
        // Rebuild wholesale every update_view — small N keeps this cheap.
        rebuild_discussions_list(
            &widgets.discussions_list,
            &widgets.discussions_header,
            &self.editor_presence,
            &sender,
        );
    }
}

/// Wipe + repopulate the sidebar Discussions list from the live presence
/// map.  Each row → click navigates to the key's detail page via the
/// same FeedActivated flow used by feed cards.
fn rebuild_discussions_list(
    list:    &gtk::ListBox,
    header:  &gtk::Label,
    presence: &HashMap<String, HashMap<[u8; 32], u64>>,
    sender:  &AsyncComponentSender<AppModel>,
) {
    // Clear all rows.
    while let Some(row) = list.first_child() {
        list.remove(&row);
    }
    let mut keys: Vec<(&String, usize)> = presence.iter()
        .filter(|(_, peers)| !peers.is_empty())
        .map(|(k, p)| (k, p.len()))
        .collect();
    keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    header.set_visible(!keys.is_empty());
    for (key, n_peers) in keys {
        let row = gtk::ListBoxRow::new();
        let outer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        outer.set_margin_start(12);
        outer.set_margin_end(12);
        outer.set_margin_top(8);
        outer.set_margin_bottom(8);
        let glyph = gtk::Label::new(Some("✎"));
        outer.append(&glyph);
        let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text_box.set_hexpand(true);
        let title = gtk::Label::new(Some(&zodia_core::humanize_key(key)));
        title.set_halign(gtk::Align::Start);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let subtitle = gtk::Label::new(Some(&format!(
            "{n_peers} editing — tap to join",
        )));
        subtitle.set_halign(gtk::Align::Start);
        subtitle.add_css_class("caption");
        subtitle.add_css_class("dim-label");
        text_box.append(&title);
        text_box.append(&subtitle);
        outer.append(&text_box);
        row.set_child(Some(&outer));
        // Click → fire FeedActivated with OpenInterpKey, reusing the nav
        // plumbing the feed cards already use.
        let key_owned = key.clone();
        let s = sender.clone();
        let click = gtk::GestureClick::new();
        click.connect_released(move |g, _, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
            let payload = crate::feed_view::ActivatedPayload::OpenInterpKey(
                key_owned.clone(),
            );
            s.input(AppMsg::FeedActivated {
                event_id: [0u8; 32],
                payload,
            });
        });
        row.add_controller(click);
        list.append(&row);
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

    let sync_status: Vec<crate::network_tab::NetSyncStatus> = model.sync_peer_status.iter()
        .map(|(pk, status)| crate::network_tab::NetSyncStatus {
            pubkey_tag: hex::encode_upper(&pk[..4]),
            label:      match status {
                SyncPeerStatus::Syncing                       => "Syncing…".to_string(),
                SyncPeerStatus::CaughtUp { received_ops: 0 }  => "Caught up".to_string(),
                SyncPeerStatus::CaughtUp { received_ops: 1 }  => "Caught up · 1 op received".to_string(),
                SyncPeerStatus::CaughtUp { received_ops: n }  => format!("Caught up · {n} ops received"),
                SyncPeerStatus::Failed   { error }            => format!("Failed: {error}"),
            },
        })
        .collect();

    let _ = s.send(crate::network_tab::NetworkTabMsg::Refresh { peers, recent, sync_status });
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

/// Always-subscribed set (Phase C-2): the keys in the user's own natal
/// chart, so the home aspect list and Sky feed stay live without needing a
/// per-page subscribe on every cold start.
async fn subscribe_own_chart_keys(chart: &Chart, client: &ZodiaClient) {
    for aspect in chart.natal_aspects() {
        let key = zodia_core::InterpKey::from_natal(&aspect).to_sig();
        if let Err(e) = client.subscribe(&key).await {
            warn!("chart-key subscribe {key}: {e}");
        }
    }
}

/// `true` if `interp_key` needs its own lazy, grace-period-limited
/// subscription when its aspect page opens (Phase C-2's non-chart case) —
/// `false` if it's already covered by [`subscribe_own_chart_keys`]'s
/// permanent, startup-time subscription to every key in the user's own
/// natal chart.
fn needs_lazy_subscription(interp_key: &str, chart: &Chart) -> bool {
    !chart.natal_aspects().iter()
        .any(|a| zodia_core::InterpKey::from_natal(a).to_sig() == interp_key)
}

#[cfg(test)]
mod subscription_lifecycle_tests {
    use super::*;

    fn sample_chart() -> Chart {
        let birth = zodia_core::birth_from_coords(2_451_545.0, 59.9, 10.7, 9);
        Chart::compute(birth).expect("chart computes for a fixed test birth")
    }

    #[test]
    fn a_key_in_the_users_own_chart_does_not_need_lazy_subscription() {
        // Given a chart with at least one natal aspect
        let chart = sample_chart();
        let aspects = chart.natal_aspects();
        assert!(!aspects.is_empty(), "test fixture should have at least one natal aspect");
        let existing_key = zodia_core::InterpKey::from_natal(&aspects[0]).to_sig();

        // When checking a key that IS one of that chart's own aspects
        // Then it does not need a lazy, grace-limited subscription — it's
        // already permanently subscribed at startup.
        assert!(!needs_lazy_subscription(&existing_key, &chart));
    }

    #[test]
    fn a_key_outside_the_users_own_chart_needs_lazy_subscription() {
        // Given a chart
        let chart = sample_chart();

        // When checking a key that is not one of the chart's own natal
        // aspects
        // Then it needs a lazy, grace-limited subscription.
        assert!(needs_lazy_subscription("natal:definitely_not_in_this_chart", &chart));
    }
}

/// Bring up the sync/doc layer via `zodia-sdk`, attached to the already-running
/// `net` (sharing its endpoint/gossip rather than spawning a second
/// `ZodiaNetwork` under the same identity — `net` stays owned by the caller
/// for Tier-1 consent/chat/AV, which this SDK doesn't cover). Bridges the
/// client's `StateEvent`/`SyncLifecycleEvent` broadcast streams into
/// `AppMsg::SyncStateEvent`/`AppMsg::SyncLifecycle` so the rest of the app
/// is unchanged from the pre-SDK wiring — see docs/prd/zodia-sdk.md.
async fn try_spawn_sync(
    config: &LocalConfig,
    net: &ZodiaNetwork,
    sender: &AsyncComponentSender<AppModel>,
) -> Option<ZodiaClient> {
    let signing_key = config.identity.signing_key().clone();
    let client = match ZodiaClient::attach(net, signing_key, config.data_dir().to_path_buf()).await {
        Ok(c) => c,
        Err(e) => { warn!("zodia-sdk client attach failed: {e}"); return None; }
    };

    let mut events = client.events();
    let sender_events = sender.clone();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => sender_events.input(AppMsg::SyncStateEvent(event)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("state event stream lagged, dropped {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut lifecycle = client.sync_lifecycle_events();
    let sender_lifecycle = sender.clone();
    tokio::spawn(async move {
        loop {
            let lifecycle_event = match lifecycle.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("sync lifecycle stream lagged, dropped {n} events");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let app_event = match lifecycle_event {
                SyncLifecycleEvent::Started { remote } =>
                    SyncLifecycle::Started { remote_pk: remote },
                SyncLifecycleEvent::Finished { remote, received_ops } =>
                    SyncLifecycle::Finished { remote_pk: remote, received_ops },
                SyncLifecycleEvent::Failed { remote, error } =>
                    SyncLifecycle::Failed { remote_pk: remote, error },
            };
            sender_lifecycle.input(AppMsg::SyncLifecycle(app_event));
        }
    });

    Some(client)
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
        discussions_list, discussions_header,
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

        // Phase E: Sky becomes the activity feed.
        let (feed_sender, nav) = mount_sky_feed(&sky_container, &sender, model.identity.public_key());
        *model.sky_nav.borrow_mut() = Some(nav);
        // Route the sender onto the model via the message bus (init has &model).
        let _ = sender.input_sender().send(AppMsg::__SetFeedSender(feed_sender));
        let me: [u8; 32] = model.identity.public_key();
        let store_bg = model.store.clone();
        let s2 = sender.clone();
        tokio::spawn(async move {
            if let Ok(rows) = store_bg.recent_feed_rows(&me, 200).await {
                let items: Vec<crate::feed_item::FeedItem> = rows.iter()
                    .map(|r| crate::feed_item::feed_row_to_feed_item(r, false))
                    .collect();
                let _ = s2.input_sender().send(AppMsg::__BackfillFeed(items));
            }
            // Initial bell badge.
            let _ = s2.input_sender().send(AppMsg::RefreshBellBadge);
        });
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
        discussions_list,
        discussions_header,
        nav_to_sky_token_shown: 0,
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
/// One-time migration: fold legacy `interpretations` rows into per-key
/// collab docs.  Baseline TOML text becomes the seed (anonymous canon);
/// each authored row appends as a paragraph attributed to its writer.
/// Affirmations on legacy rows fold into doc_affirms against the final
/// revision after migration.  Idempotent via the `collab_doc_migration_v1`
/// feed_meta flag.
async fn migrate_interps_to_docs(
    store:    &ZodiaStore,
    baseline: &BaselineStore,
    me:       &[u8; 32],
) -> Result<(), StoreError> {
    if store.collab_doc_migration_done().await? {
        return Ok(());
    }
    let me_vk = match zodia_doc::VerifyingKey::from_bytes(me) {
        Ok(vk) => vk,
        Err(_) => return Ok(()),
    };
    let keys = store.distinct_interp_keys().await?;
    info!(n_keys = keys.len(), "collab-doc migration starting");

    for key in keys {
        let mut body = String::new();
        // Seed: baseline text (anonymous canon).
        if let Some(parsed) = zodia_core::parse_interp_sig(&key) {
            if let Some(base) = baseline.lookup(&parsed) {
                body.push_str(base);
            }
        }
        let rows = store.authored_rows_for_key(&key).await?;
        for (text, _author, _ts) in &rows {
            if !body.is_empty() { body.push_str("\n\n"); }
            body.push_str(text);
        }
        if body.is_empty() { continue; }

        let doc = zodia_doc::InterpDoc::empty(&me_vk);
        if doc.set_body(&body).is_err() { continue; }
        let edit = match doc.publish_local() { Ok(e) => e, Err(_) => continue };
        let snap = match doc.snapshot()      { Ok(b) => b, Err(_) => continue };
        store.doc_save(&key, &snap, &edit.rev).await?;

        // Attribute each authored row to its writer in the block ring.
        // Use a synthetic edit_op_id derived from (author, ts) so vetoes
        // can target the migration-seeded edits even though there's no
        // p2panda op behind them.
        for (text, author, ts) in &rows {
            let mut h = blake3::Hasher::new();
            h.update(b"migration-edit");
            h.update(author);
            h.update(text.as_bytes());
            let synthetic_op_id = *h.finalize().as_bytes();
            store.block_ring_push(
                &key, &zodia_doc::BODY_BLOCK_ID, author, &synthetic_op_id, *ts,
            ).await?;
        }
    }

    store.mark_collab_doc_migration_done().await?;
    info!("collab-doc migration complete");
    Ok(())
}

/// Op-id helper: the pipeline's StateEvent variants carry `op_id` as a
/// p2panda Hash; we need it as `[u8; 32]` for store keying.
fn op_id_for(ev: &StateEvent) -> [u8; 32] {
    match ev {
        StateEvent::DocEdited { op_id, .. }
        | StateEvent::DocVetoProposed { op_id, .. }
        | StateEvent::DocAffirmed { op_id, .. }
        | StateEvent::EditorPresenceChanged { op_id, .. }
        | StateEvent::InterpAuthored { op_id, .. }
        | StateEvent::ResponseAdded { op_id, .. }
        | StateEvent::InterpRevoked { op_id, .. } => *op_id.as_bytes(),
        StateEvent::AffirmAdded { .. } | StateEvent::Skipped { .. } => [0u8; 32],
    }
}

/// Apply a remote CRDT update to the per-key doc.  Loads (or creates) the
/// doc, captures the pre-edit snapshot, applies the update, persists the
/// new snapshot with rollback metadata, and pushes the editor onto each
/// affected block's ring.  Self-edits skip ring push (you're not on your
/// own veto ring).
async fn apply_doc_edit(
    store:           &ZodiaStore,
    interp_key:      &str,
    update:          &[u8],
    editor_pk:       &[u8],
    edit_op_id:      &[u8; 32],
    edit_ts:         u64,
    affected_blocks: &[[u8; 16]],
    me:              &[u8; 32],
) -> Result<(), StoreError> {
    let mut bytes = [0u8; 32];
    if editor_pk.len() != 32 { return Ok(()); }
    bytes.copy_from_slice(editor_pk);
    let editor_vk = match zodia_doc::VerifyingKey::from_bytes(&bytes) {
        Ok(vk) => vk,
        Err(_) => return Ok(()),
    };
    let me_vk = zodia_doc::VerifyingKey::from_bytes(me).unwrap_or(editor_vk);

    let prior_bytes = store.doc_load(interp_key).await?;
    let doc = match &prior_bytes {
        Some(bytes) => {
            match zodia_doc::InterpDoc::from_snapshot(&me_vk, bytes) {
                Ok(d)  => d,
                Err(e) => { warn!("doc snapshot restore failed: {e}"); return Ok(()); }
            }
        }
        None => zodia_doc::InterpDoc::empty(&me_vk),
    };
    if let Err(e) = doc.apply_remote(update) {
        warn!("doc CRDT apply failed: {e}");
        return Ok(());
    }
    let snapshot = match doc.snapshot() {
        Ok(b) => b,
        Err(e) => { warn!("doc snapshot failed: {e}"); return Ok(()); }
    };
    let rev = doc.current_rev();
    let mut editor_arr = [0u8; 32];
    editor_arr.copy_from_slice(editor_pk);
    let blocks_cbor = encode_blocks(affected_blocks);
    store.doc_save_with_history(
        interp_key,
        &snapshot,
        &rev,
        prior_bytes.as_deref(),
        edit_op_id,
        edit_ts,
        &editor_arr,
        &blocks_cbor,
    ).await?;

    // Push editor onto each affected block's ring (skip self).
    if editor_pk != me.as_slice() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for block_id in affected_blocks {
            store.block_ring_push(
                interp_key, block_id, &editor_arr, edit_op_id, now,
            ).await?;
        }
    }
    Ok(())
}

/// Authority check + rollback for a `DocOp::Veto`.  Returns `Some((author,
/// rev))` of the rolled-back edit if the veto was honoured, `None` otherwise.
/// `author` lets callers attribute the resulting feed event.
async fn apply_doc_veto(
    store:             &ZodiaStore,
    target_edit_op_id: &[u8; 32],
    by:                &[u8; 32],
    interp_key_hint:   Option<&str>,
    now_ts:            u64,
) -> Option<(String, [u8; 32], [u8; 32])> {
    // The wire op carries only the target_edit_op_id, not the interp_key.
    // Caller can pass a hint when known (local veto path); otherwise we'd
    // need a (interp_key) lookup table — for the tracer's single-doc case
    // the local path always knows the key and remote vetoes are paired with
    // a sender on the same channel, so this hint suffices.
    let key = interp_key_hint?.to_string();
    let meta = store.doc_load_meta(&key).await.ok().flatten()?;
    if meta.last_edit_op_id.as_ref() != Some(target_edit_op_id) { return None; }
    let edit_ts = meta.last_edit_ts?;
    let edit_author = meta.last_edit_author?;
    let blocks = decode_blocks(meta.last_edit_blocks.as_deref().unwrap_or(&[]));
    if blocks.is_empty() { return None; }
    // Authority: revoker must be in the ring for at least one affected
    // block, within the veto window, targeting the newest edit.
    let mut authorised = false;
    for block_id in &blocks {
        let entries = store.block_ring_get(&key, block_id).await.ok()
            .unwrap_or_default();
        let ring_entries: Vec<zodia_doc::RingEntry> = entries.into_iter()
            .map(|(a, e, t)| zodia_doc::RingEntry {
                author: a, edit_op_id: e, edited_at: t,
            })
            .collect();
        let ring = zodia_doc::Ring::from_entries(ring_entries);
        if zodia_doc::veto_authorised(&ring, by, target_edit_op_id, edit_ts, now_ts) {
            authorised = true;
            break;
        }
    }
    if !authorised { return None; }

    // Roll back: restore prior snapshot, pop ring newest per block.  Compute
    // the post-rollback rev from the restored snapshot for store metadata.
    let prior = meta.prior_snapshot?;
    let me_vk = zodia_doc::VerifyingKey::from_bytes(by).ok()?;
    let restored = zodia_doc::InterpDoc::from_snapshot(&me_vk, &prior).ok()?;
    let rev = restored.current_rev();
    if store.doc_rollback(&key, &rev, &blocks).await.ok()? {
        Some((key, edit_author, *target_edit_op_id))
    } else {
        None
    }
}

/// Drop presence entries whose last-seen heartbeat is older than
/// `PRESENCE_TTL_SECS`.  Called before any consumer reads the map (start
/// audio, render presence chips) so the user never sees stale peers.
fn prune_stale_presence(map: &mut HashMap<String, HashMap<[u8; 32], u64>>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now.saturating_sub(PRESENCE_TTL_SECS);
    map.retain(|_, peers| {
        peers.retain(|_, last_seen| *last_seen >= cutoff);
        !peers.is_empty()
    });
}

/// Live-refresh helper: load the doc, decode body, push into the currently-
/// visible TextBuffer if the user is on that key's page.  No-op otherwise.
async fn push_doc_body_refresh(store: &ZodiaStore, interp_key: &str, me: &[u8; 32]) {
    let Ok(Some(snap)) = store.doc_load(interp_key).await else { return; };
    let Ok(vk) = zodia_doc::VerifyingKey::from_bytes(me) else { return; };
    let Ok(d) = zodia_doc::InterpDoc::from_snapshot(&vk, &snap) else { return; };
    crate::aspect_view::refresh_active_doc_body(interp_key, &d.body_text());
}

fn encode_blocks(blocks: &[[u8; 16]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(blocks.len() * 16);
    for b in blocks { out.extend_from_slice(b); }
    out
}

fn decode_blocks(bytes: &[u8]) -> Vec<[u8; 16]> {
    bytes.chunks_exact(16).map(|c| {
        let mut b = [0u8; 16]; b.copy_from_slice(c); b
    }).collect()
}

/// Look up the interp_key the affirmed row belongs to, plus whether the
/// affirmation targets `me`.
async fn affirm_lookup(
    store:         &ZodiaStore,
    target_log_id: &[u8; 32],
    voter:         &[u8; 32],
    me:            &[u8; 32],
) -> (String, bool) {
    match store.interp_key_and_author(target_log_id).await {
        Ok(Some((key, author))) => {
            let authored_by_me = author.as_ref().map(|a| a == me).unwrap_or(false);
            (key, authored_by_me && voter != me)
        }
        _ => (String::new(), false),
    }
}

/// Look up the parent's interp_key + whether the response targets `me`.
async fn response_parent_lookup(
    store:           &ZodiaStore,
    parent_log_id:   &[u8; 32],
    response_author: &[u8; 32],
    me:              &[u8; 32],
) -> (String, bool) {
    match store.interp_key_and_author(parent_log_id).await {
        Ok(Some((key, author))) => {
            let authored_by_me = author.as_ref().map(|a| a == me).unwrap_or(false);
            (key, authored_by_me && response_author != me)
        }
        _ => (String::new(), false),
    }
}

/// Spawn the transit ticker for `chart` and a relay loop that forwards its
/// `FeedItem`s into the app's message bus.  Returns a supervisor handle
/// the caller can `abort()` on chart change.
fn spawn_transit_ticker(
    chart:    Chart,
    baseline: Rc<BaselineStore>,
    store:    ZodiaStore,
    sender:   relm4::component::AsyncComponentSender<AppModel>,
) -> tokio::task::JoinHandle<()> {
    // BaselineStore needs to cross into the spawned task — clone its
    // `Arc`-backed inner state via the existing constructor.  `Rc` here
    // (from AppModel) holds the same data; reconstitute it into a fresh
    // `Arc` for the !Send-free move.
    let baseline_arc = std::sync::Arc::new((*baseline).clone());
    tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::feed_item::FeedItem>(64);
        let chart_arc = std::sync::Arc::new(chart);
        let _ticker = crate::transit_ticker::spawn(chart_arc, baseline_arc, store, tx);
        while let Some(item) = rx.recv().await {
            let _ = sender.input_sender().send(AppMsg::__PushFeedItem(item));
        }
    })
}

/// Spawn the Sky-tab FeedView and append it to `sky_container`.  Returns
/// the input sender so the caller can stash it on the model for live
/// updates.  Idempotent at the call site — the caller guards on
/// `sky_container.first_child().is_none()`.
fn mount_sky_feed(
    sky_container: &gtk::Box,
    parent_sender: &relm4::component::AsyncComponentSender<AppModel>,
    me:            [u8; 32],
) -> (relm4::Sender<crate::feed_view::FeedViewMsg>, adw::NavigationView) {
    let (widget, sender) = crate::feed_view::launch(
        crate::feed_view::FeedViewInit { filter_key: None, initial: Vec::new(), me },
        parent_sender.input_sender(),
        |out| match out {
            crate::feed_view::FeedViewOut::Activate { event_id, payload } =>
                AppMsg::FeedActivated { event_id, payload },
            crate::feed_view::FeedViewOut::Revoke { log_id } =>
                AppMsg::SubmitRevoke { log_id },
            crate::feed_view::FeedViewOut::SetRead { event_id, read } =>
                AppMsg::FeedSetRead { event_id, read },
        },
    );
    widget.set_vexpand(true);

    // Wrap the FeedView inside a NavigationView so a card-click can push a
    // detail page on top of the feed (back arrow returns to it).  The root
    // page is the feed itself.
    let nav = adw::NavigationView::new();
    let root_page = adw::NavigationPage::new(&widget, "Sky");
    nav.add(&root_page);
    nav.set_vexpand(true);
    sky_container.append(&nav);

    (sender, nav)
}

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
    gtk::ListBox, gtk::Label,                           // discussions_list, discussions_header
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

    // Phase F-collab: sidebar Discussions list — populated dynamically from
    // editor_presence on update_view.  Header hides when list is empty.
    let discussions_list = gtk::ListBox::new();
    discussions_list.set_selection_mode(gtk::SelectionMode::None);
    discussions_list.add_css_class("navigation-sidebar");
    let discussions_header = gtk::Label::new(Some("Discussions"));
    discussions_header.add_css_class("heading");
    discussions_header.add_css_class("dim-label");
    discussions_header.set_halign(gtk::Align::Start);
    discussions_header.set_margin_start(12);
    discussions_header.set_margin_end(12);
    discussions_header.set_margin_top(12);
    discussions_header.set_margin_bottom(2);
    discussions_header.set_visible(false);

    // Sidebar — Component owning Zodia header, NotifBell pack, nav list, peers slot.
    let (sidebar_toolbar, sidebar_sender) = crate::sidebar::launch(crate::sidebar::SidebarInit {
        peers_list:         peers_list.clone(),
        discussions_list:   discussions_list.clone(),
        discussions_header: discussions_header.clone(),
        notif_widget:       notif_widget.clone(),
        split_view:         split_view.clone(),
        content_stack:      content_stack.clone(),
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
        discussions_list, discussions_header,
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
