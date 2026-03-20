//! Root application component.
//!
//! `AppModel` is an `AsyncComponent` that drives the full lifecycle:
//!   1. First-run setup  — collect birth date + location, compute chart
//!   2. Main view        — Chart / Sky / Peers tabs in an `adw::NavigationView`
//!   3. Connected peer   — pushed `adw::NavigationPage` with synastry + call
//!   4. Network events   — `CommandOutput = ZodiaNetEvent` keeps the peer
//!                         list reactive without blocking the GTK thread

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::prelude::*;
use relm4::factory::FactoryVecDeque;
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
use crate::peer_list::{PeerEntry, PeerInit, PeerOutput};
use crate::peer_page;
use crate::util::approximate_aspects;

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
    Calling { peer_id: PeerId },
    Ringing { peer_id: PeerId, session_id: [u8; 32] },
    Active  { peer_id: PeerId },
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
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        lat: f64,
        lon: f64,
    },
    SetupError(String),
    ConnectPeer(PeerId),
    CallPeer(PeerId),
    AcceptCall,
    RejectCall,
    HangUp,
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct AppModel {
    on_setup_page: bool,
    chart: Option<Chart>,

    /// Shared store — Rc so GTK signal closures can borrow it.
    store: Rc<RefCell<ZodiaStore>>,

    network: Option<ZodiaNetwork>,
    peer_count: usize,
    node_id_text: String,
    peers: FactoryVecDeque<PeerEntry>,
    config: LocalConfig,
    setup_error: String,

    /// Identity keypair — signs submitted interpretations and affirmations.
    identity: Rc<IdentityKeypair>,

    call_state: CallState,
    connected_channels: HashMap<PeerId, DirectChannel>,

    /// Peers whose Tier-1 exchange has completed — drives peer page creation.
    connected_peers: HashMap<PeerId, Tier1Blob>,

    active_audio: Option<AudioSession>,
}

// ── widgets ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct AppWidgets {
    outer_stack: gtk::Stack,
    setup_status: gtk::Label,

    /// Container for the natal `AspectView` — lazily populated.
    chart_container: gtk::Box,
    /// Container for the transit `AspectView` — lazily populated.
    sky_container: gtk::Box,

    peers_page: adw::ViewStackPage,
    peer_count_label: gtk::Label,
    node_id_label: gtk::Label,

    call_bar: gtk::Box,
    call_status: gtk::Label,
    accept_btn: gtk::Button,
    hangup_btn: gtk::Button,

    /// NavigationView holding the main page + pushed peer pages.
    nav_view: adw::NavigationView,
    /// Peers already pushed as navigation pages (avoids duplicates).
    shown_peers: HashSet<PeerId>,
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
        let peers = FactoryVecDeque::builder()
            .launch(gtk::ListBox::new())
            .forward(sender.input_sender(), |msg| match msg {
                PeerOutput::Connect(id) => AppMsg::ConnectPeer(id),
                PeerOutput::Call(id)    => AppMsg::CallPeer(id),
            });

        let identity = Rc::new(IdentityKeypair::from_seed(init.config.identity.seed()));
        let has_birth = init.config.birth.is_some();
        let store = Rc::new(RefCell::new(init.store));

        let mut model = AppModel {
            on_setup_page: !has_birth,
            chart: None,
            store,
            network: None,
            peer_count: 0,
            node_id_text: String::new(),
            peers,
            config: init.config,
            setup_error: String::new(),
            identity,
            call_state: CallState::Idle,
            connected_channels: HashMap::new(),
            connected_peers: HashMap::new(),
            active_audio: None,
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
                let jdn = gregorian_to_jdn(
                    year, month, day,
                    hour as f64 + minute as f64 / 60.0,
                );
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

            AppMsg::ConnectPeer(peer_id) => {
                if let Some(net) = &self.network {
                    match net.connect_peer(&peer_id).await {
                        Ok(channel) => {
                            let peer_hex = hex::encode_upper(&peer_id.0[..4]);
                            info!(peer = %peer_hex, "tier-1 channel opened");

                            if let Some(our_blob) = make_tier1_blob(&self.config) {
                                match channel.exchange_tier1(&our_blob).await {
                                    Ok(their_blob) => {
                                        info!(peer = %peer_hex, "tier-1 exchange complete");
                                        // Sync interpretations for their keys before
                                        // handing the channel to the message loop.
                                        do_interp_sync(
                                            &channel, &their_blob,
                                            self.chart.as_ref(), &self.store,
                                            &self.identity, &peer_hex,
                                        ).await;
                                        self.connected_peers.insert(peer_id.clone(), their_blob);
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
                let approx = self
                    .chart
                    .as_ref()
                    .map(|c| approximate_aspects(blob.solar_month, &c.positions))
                    .unwrap_or_default();

                self.peers.guard().push_back(PeerInit {
                    peer_id,
                    geohash_prefix: blob.geohash_prefix,
                    solar_month: blob.solar_month,
                    approximate_aspects: approx,
                });
                self.peer_count += 1;
            }
            ZodiaNetEvent::PeerLeft { peer_id } => {
                let maybe_idx = self
                    .peers
                    .guard()
                    .iter()
                    .position(|p| p.peer_id == peer_id);
                if let Some(i) = maybe_idx {
                    self.peers.guard().remove(i);
                    self.peer_count = self.peer_count.saturating_sub(1);
                }
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
            _ => {}
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, sender: AsyncComponentSender<Self>) {
        if self.on_setup_page {
            widgets.outer_stack.set_visible_child_name("setup");
        } else {
            widgets.outer_stack.set_visible_child_name("main");
        }
        widgets.setup_status.set_text(&self.setup_error);

        // Lazily populate aspect views the first time the main page is shown.
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

        // Push a navigation page for each newly connected peer.
        for (peer_id, their_blob) in &self.connected_peers {
            if !widgets.shown_peers.contains(peer_id) {
                if let Some(chart) = &self.chart {
                    let page = peer_page::build_peer_page(
                        peer_id,
                        their_blob,
                        chart,
                        Rc::clone(&self.store),
                        Rc::clone(&self.identity),
                        &sender,
                    );
                    widgets.nav_view.push(&page);
                    widgets.shown_peers.insert(peer_id.clone());
                }
            }
        }

        // Peer count + badge
        let peer_text = if self.peer_count == 0 {
            "Scanning for peers…".to_string()
        } else {
            format!(
                "{} peer{} online",
                self.peer_count,
                if self.peer_count == 1 { "" } else { "s" }
            )
        };
        widgets.peer_count_label.set_text(&peer_text);
        widgets.peers_page.set_needs_attention(self.peer_count > 0);

        if !self.node_id_text.is_empty() {
            widgets.node_id_label.set_text(&format!("Node ···{}", self.node_id_text));
        }

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

// ── helpers ───────────────────────────────────────────────────────────────────

async fn try_spawn_network(
    config: &LocalConfig,
    birth: &zodia_core::BirthData,
) -> Option<(ZodiaNetwork, Receiver<ZodiaNetEvent>)> {
    let signing_key = config.identity.signing_key().clone();
    match ZodiaNetwork::spawn(NetworkConfig { signing_key }, birth).await {
        Ok(pair) => Some(pair),
        Err(e) => {
            error!("network spawn failed: {e}");
            None
        }
    }
}

fn start_network_command(
    sender: &AsyncComponentSender<AppModel>,
    rx: Receiver<ZodiaNetEvent>,
) {
    sender.command(|out, _shutdown| async move {
        let mut rx: Receiver<ZodiaNetEvent> = rx;
        while let Some(ev) = rx.recv().await {
            if out.send(ev).is_err() {
                break;
            }
        }
    });
}

/// Construct our Tier-1 blob with exact birth data.
///
/// Prekey/ephemeral fields are zeroed — crypto key exchange is deferred
/// until message support is added (see zodia-crypto).
fn make_tier1_blob(config: &LocalConfig) -> Option<Tier1Blob> {
    config.birth.as_ref().map(|birth| Tier1Blob {
        birth: birth.clone(),
        prekey:   [0u8; 32], // TODO: real X25519 prekey
        ephemeral: [0u8; 32], // TODO: real ephemeral key for X3DH
    })
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

    let (nav_view, chart_container, sky_container, peers_page, peer_count_label,
         node_id_label, peers_scrolled, call_bar, call_status, accept_btn, hangup_btn) =
        build_main_page(model, sender);
    outer_stack.add_named(&nav_view, Some("main"));

    peers_scrolled.set_child(Some(model.peers.widget()));

    // For returning users with an existing chart, populate aspect views now.
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

    if model.on_setup_page {
        outer_stack.set_visible_child_name("setup");
    } else {
        outer_stack.set_visible_child_name("main");
    }

    root.set_content(Some(&outer_stack));

    AppWidgets {
        outer_stack,
        setup_status,
        chart_container,
        sky_container,
        peers_page,
        peer_count_label,
        node_id_label,
        call_bar,
        call_status,
        accept_btn,
        hangup_btn,
        nav_view,
        shown_peers: HashSet::new(),
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
    adw::NavigationView,
    gtk::Box, gtk::Box,               // chart_container, sky_container
    adw::ViewStackPage, gtk::Label,   // peers_page, peer_count_label
    gtk::Label,                       // node_id_label
    gtk::ScrolledWindow,              // peers_scrolled
    gtk::Box, gtk::Label, gtk::Button, gtk::Button, // call bar widgets
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

    // ── Peers tab ─────────────────────────────────────────────────────────────
    let peers_container = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let peer_count_label = gtk::Label::new(Some("Scanning for peers…"));
    peer_count_label.add_css_class("dim-label");
    peer_count_label.add_css_class("caption");
    peer_count_label.set_halign(gtk::Align::Start);
    peer_count_label.set_margin_start(12);
    peer_count_label.set_margin_end(12);
    peer_count_label.set_margin_top(10);
    peer_count_label.set_margin_bottom(4);
    peers_container.append(&peer_count_label);

    let hint = gtk::Label::new(Some(
        "All online Zodia peers appear below.\n\
         Aspect glyphs show their ☉ resonance with your natal chart.",
    ));
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");
    hint.set_wrap(true);
    hint.set_halign(gtk::Align::Start);
    hint.set_margin_start(12);
    hint.set_margin_end(12);
    hint.set_margin_bottom(8);
    peers_container.append(&hint);

    let peers_scrolled = gtk::ScrolledWindow::new();
    peers_scrolled.set_vexpand(true);
    peers_scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    peers_container.append(&peers_scrolled);

    let peers_page = view_stack.add_titled(&peers_container, Some("peers"), "Peers");
    peers_page.set_icon_name(Some("system-users-symbolic"));

    toolbar_view.set_content(Some(&view_stack));

    // ── Header bar ────────────────────────────────────────────────────────────
    let switcher_title = adw::ViewSwitcherTitle::new();
    switcher_title.set_stack(Some(&view_stack));
    switcher_title.set_title("Zodia");

    let node_id_label = gtk::Label::new(Some("Node ···----"));
    node_id_label.add_css_class("node-id");
    node_id_label.add_css_class("dim-label");

    let header_bar = adw::HeaderBar::new();
    header_bar.set_title_widget(Some(&switcher_title));
    header_bar.pack_end(&node_id_label);
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

    // Wrap in NavigationView so peer pages can be pushed onto the stack.
    let nav_view = adw::NavigationView::new();
    nav_view.push(&adw::NavigationPage::new(&toolbar_view, "Zodia"));

    let _ = model;

    (nav_view, chart_container, sky_container, peers_page, peer_count_label,
     node_id_label, peers_scrolled, call_bar, call_status, accept_btn, hangup_btn)
}

// ── interpretation sync ───────────────────────────────────────────────────────

/// Run a mutual interpretation exchange over `channel`.
///
/// We collect our top signed community entries for the peer's relevant keys,
/// send them, receive theirs, and import any we haven't seen.  Must be called
/// before `accept_channel` so the streams aren't grabbed by the message loop.
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

/// Build the list of our signed community entries for the keys a peer cares
/// about: their natal aspect keys plus synastry keys between our charts.
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
    store
        .borrow()
        .community_for_keys(&refs, 100)
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

/// Verify and insert received interpretations.  Returns the number newly stored.
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
            &entry.interp_key,
            &entry.body,
            &entry.author_pk,
            &sig_arr,
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
