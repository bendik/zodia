//! Root application component.
//!
//! `AppModel` is an `AsyncComponent` that drives the full lifecycle:
//!   1. First-run setup  — collect birth date + location, compute chart
//!   2. Main view        — display natal aspects, current transits, and
//!                         peer discovery with approximate synastry glyphs
//!   3. Network events   — `CommandOutput = ZodiaNetEvent` keeps the peer
//!                         list reactive without blocking the GTK thread

use std::collections::HashMap;

use gtk::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use tokio::sync::mpsc::Receiver;
use tracing::{error, info};
use zodia_av::AudioSession;
use zodia_config::LocalConfig;
use zodia_core::{birth_from_coords, current_jdn, gregorian_to_jdn, Chart};
use zodia_net::{ChannelMsg, DirectChannel, NetworkConfig, PeerId, ZodiaNetEvent, ZodiaNetwork};
use zodia_store::ZodiaStore;

use crate::peer_list::{PeerEntry, PeerInit, PeerOutput};
use crate::util::{approximate_aspects, format_aspect_card, format_transit_card, format_house_transit_card};

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
    /// We sent a CallOffer and are waiting for the remote to accept.
    Calling { peer_id: PeerId },
    /// The remote sent us a CallOffer; user needs to accept or reject.
    Ringing { peer_id: PeerId, session_id: [u8; 32] },
    /// Audio is flowing.
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
    // first-run form
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
    // peer list
    ConnectPeer(PeerId),
    // call lifecycle
    CallPeer(PeerId),
    AcceptCall,
    RejectCall,
    HangUp,
}

// ── model ─────────────────────────────────────────────────────────────────────

pub struct AppModel {
    // navigation
    on_setup_page: bool,

    // chart & transits (populated once birth is known)
    natal_text: String,
    transit_text: String,
    chart: Option<Chart>,

    // store
    store: ZodiaStore,

    // network
    network: Option<ZodiaNetwork>,
    peer_count: usize,
    node_id_text: String,

    // peer list (FactoryVecDeque owns the gtk::ListBox)
    peers: FactoryVecDeque<PeerEntry>,

    // mutable config (for saving birth data on first run)
    config: LocalConfig,

    // status
    setup_error: String,

    // call state
    call_state: CallState,
    connected_channels: HashMap<PeerId, DirectChannel>,
    active_audio: Option<AudioSession>,
}

// ── widgets ───────────────────────────────────────────────────────────────────

// Widget references are kept for lifetime management; spin/entry fields are
// read through clones wired up in signal handlers, not through this struct.
#[allow(dead_code)]
pub struct AppWidgets {
    // ── setup page ────────────────────────────────────────────────────────────
    stack: gtk::Stack,
    year_spin: gtk::SpinButton,
    month_spin: gtk::SpinButton,
    day_spin: gtk::SpinButton,
    hour_spin: gtk::SpinButton,
    minute_spin: gtk::SpinButton,
    lat_entry: gtk::Entry,
    lon_entry: gtk::Entry,
    setup_status: gtk::Label,

    // ── main page ─────────────────────────────────────────────────────────────
    natal_label: gtk::Label,
    transit_label: gtk::Label,
    peer_count_label: gtk::Label,
    node_id_label: gtk::Label,

    // ── call bar (shown when a call is active/ringing/calling) ────────────────
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
    type Root = gtk::ApplicationWindow;
    type Widgets = AppWidgets;

    fn init_root() -> Self::Root {
        gtk::ApplicationWindow::new(&relm4::main_application())
    }

    async fn init(
        init: Self::Init,
        root: Self::Root,
        sender: AsyncComponentSender<Self>,
    ) -> AsyncComponentParts<Self> {
        // ── peers factory ─────────────────────────────────────────────────────
        let peers = FactoryVecDeque::builder()
            .launch(gtk::ListBox::new())
            .forward(sender.input_sender(), |msg| match msg {
                PeerOutput::Connect(id) => AppMsg::ConnectPeer(id),
                PeerOutput::Call(id)    => AppMsg::CallPeer(id),
            });

        // ── model ─────────────────────────────────────────────────────────────
        let has_birth = init.config.birth.is_some();
        let mut model = AppModel {
            on_setup_page: !has_birth,
            natal_text: String::new(),
            transit_text: String::new(),
            chart: None,
            store: init.store,
            network: None,
            peer_count: 0,
            node_id_text: String::new(),
            peers,
            config: init.config,
            setup_error: String::new(),
            call_state: CallState::Idle,
            connected_channels: HashMap::new(),
            active_audio: None,
        };

        // ── compute chart if birth is already known ───────────────────────────
        if let Some(birth) = model.config.birth.clone() {
            if let Ok(chart) = Chart::compute(birth.clone()) {
                let jdn = current_jdn();
                model.natal_text  = build_natal_text(&chart, &model.store);
                model.transit_text = build_transit_text(&chart, jdn, &model.store);
                model.chart = Some(chart);
            }
        }

        // ── build all GTK widgets ─────────────────────────────────────────────
        let widgets = build_widgets(&root, &model, &sender);

        // ── spawn network if birth is known ───────────────────────────────────
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
                    Ok(chart) => {
                        let now = current_jdn();
                        self.natal_text   = build_natal_text(&chart, &self.store);
                        self.transit_text = build_transit_text(&chart, now, &self.store);
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
                            info!(peer = %hex::encode_upper(&peer_id.0[..4]), "tier-1 channel opened");
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
                // sequential guards: first read, then write
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

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: AsyncComponentSender<Self>) {
        if self.on_setup_page {
            widgets.stack.set_visible_child_name("setup");
        } else {
            widgets.stack.set_visible_child_name("main");
        }
        widgets.setup_status.set_text(&self.setup_error);
        widgets.natal_label.set_text(&self.natal_text);
        widgets.transit_label.set_text(&self.transit_text);
        widgets.peer_count_label.set_text(&if self.peer_count == 0 {
            "Scanning for peers…".to_string()
        } else {
            format!(
                "{} peer{} nearby",
                self.peer_count,
                if self.peer_count == 1 { "" } else { "s" }
            )
        });
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

// ── widget construction ───────────────────────────────────────────────────────

fn build_widgets(
    root: &gtk::ApplicationWindow,
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) -> AppWidgets {
    root.set_title(Some("Zodia"));
    root.set_default_size(1000, 680);

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(200);

    let (setup_page, year_spin, month_spin, day_spin, hour_spin, minute_spin,
         lat_entry, lon_entry, setup_status) = build_setup_page(sender);
    stack.add_named(&setup_page, Some("setup"));

    let (main_page, natal_label, transit_label, peer_count_label, node_id_label,
         peers_scrolled, call_bar, call_status, accept_btn, hangup_btn) =
        build_main_page(model, sender);
    stack.add_named(&main_page, Some("main"));

    peers_scrolled.set_child(Some(model.peers.widget()));
    model.peers.widget().set_css_classes(&["peer-list"]);

    if model.on_setup_page {
        stack.set_visible_child_name("setup");
    } else {
        stack.set_visible_child_name("main");
    }

    root.set_child(Some(&stack));

    AppWidgets {
        stack,
        year_spin, month_spin, day_spin, hour_spin, minute_spin,
        lat_entry, lon_entry, setup_status,
        natal_label, transit_label, peer_count_label, node_id_label,
        call_bar, call_status, accept_btn, hangup_btn,
    }
}

#[allow(clippy::type_complexity)]
fn build_setup_page(
    sender: &AsyncComponentSender<AppModel>,
) -> (
    gtk::Box,
    gtk::SpinButton, gtk::SpinButton, gtk::SpinButton,
    gtk::SpinButton, gtk::SpinButton,
    gtk::Entry, gtk::Entry,
    gtk::Label,
) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let inner = gtk::Box::new(gtk::Orientation::Vertical, 16);
    inner.set_halign(gtk::Align::Center);
    inner.set_valign(gtk::Align::Center);
    inner.set_vexpand(true);
    inner.set_margin_start(40);
    inner.set_margin_end(40);
    outer.append(&inner);

    let title = gtk::Label::new(Some("Welcome to Zodia"));
    title.set_css_classes(&["setup-title"]);
    inner.append(&title);

    let subtitle = gtk::Label::new(Some(
        "Enter your birth details to find your astrological neighbourhood.",
    ));
    subtitle.set_css_classes(&["setup-subtitle"]);
    subtitle.set_wrap(true);
    subtitle.set_max_width_chars(50);
    inner.append(&subtitle);

    // date/time grid
    let grid = gtk::Grid::new();
    grid.set_column_spacing(8);
    grid.set_row_spacing(6);
    grid.set_halign(gtk::Align::Center);

    let lbl = |t: &str| -> gtk::Label {
        let l = gtk::Label::new(Some(t));
        l.set_halign(gtk::Align::Start);
        l.set_css_classes(&["field-label"]);
        l
    };

    let year_spin  = spin(1900.0, 2100.0, 1990.0);
    let month_spin = spin(1.0,    12.0,   6.0);
    let day_spin   = spin(1.0,    31.0,   15.0);
    let hour_spin  = spin(0.0,    23.0,   12.0);
    let min_spin   = spin(0.0,    59.0,   0.0);

    grid.attach(&lbl("Year"),     0, 0, 1, 1);
    grid.attach(&year_spin,       1, 0, 1, 1);
    grid.attach(&lbl("Month"),    2, 0, 1, 1);
    grid.attach(&month_spin,      3, 0, 1, 1);
    grid.attach(&lbl("Day"),      4, 0, 1, 1);
    grid.attach(&day_spin,        5, 0, 1, 1);
    grid.attach(&lbl("Hour UTC"), 0, 1, 1, 1);
    grid.attach(&hour_spin,       1, 1, 1, 1);
    grid.attach(&lbl("Minute"),   2, 1, 1, 1);
    grid.attach(&min_spin,        3, 1, 1, 1);
    inner.append(&grid);

    // location row
    let loc = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    loc.set_halign(gtk::Align::Center);

    let lat_entry = entry("Latitude  e.g. 51.5");
    let lon_entry = entry("Longitude  e.g. -0.1");
    loc.append(&lbl("Latitude"));
    loc.append(&lat_entry);
    loc.append(&lbl("Longitude"));
    loc.append(&lon_entry);
    inner.append(&loc);

    let setup_status = gtk::Label::new(None);
    setup_status.set_css_classes(&["setup-error"]);
    inner.append(&setup_status);

    let btn = gtk::Button::with_label("Begin  →");
    btn.set_css_classes(&["suggested-action", "confirm-btn"]);
    btn.set_halign(gtk::Align::Center);
    inner.append(&btn);

    // wire up
    let s = sender.clone();
    let (ys, ms, ds, hs, mins, late, lone) = (
        year_spin.clone(), month_spin.clone(), day_spin.clone(),
        hour_spin.clone(), min_spin.clone(),
        lat_entry.clone(), lon_entry.clone(),
    );
    btn.connect_clicked(move |_| {
        let lat = match late.text().parse::<f64>() {
            Ok(v) => v,
            Err(_) => { s.input(AppMsg::SetupError("Invalid latitude".into())); return; }
        };
        let lon = match lone.text().parse::<f64>() {
            Ok(v) => v,
            Err(_) => { s.input(AppMsg::SetupError("Invalid longitude".into())); return; }
        };
        s.input(AppMsg::ConfirmBirth {
            year:   ys.value() as i32,
            month:  ms.value() as u32,
            day:    ds.value() as u32,
            hour:   hs.value() as u32,
            minute: mins.value() as u32,
            lat, lon,
        });
    });

    (outer, year_spin, month_spin, day_spin, hour_spin, min_spin, lat_entry, lon_entry, setup_status)
}

#[allow(clippy::type_complexity)]
fn build_main_page(
    model: &AppModel,
    sender: &AsyncComponentSender<AppModel>,
) -> (gtk::Box, gtk::Label, gtk::Label, gtk::Label, gtk::Label,
      gtk::ScrolledWindow, gtk::Box, gtk::Label, gtk::Button, gtk::Button) {
    let root_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);

    // ── chart panel ───────────────────────────────────────────────────────────
    let chart_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    chart_box.set_css_classes(&["chart-panel"]);
    chart_box.set_width_request(340);

    let chart_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    chart_header.set_css_classes(&["panel-header"]);
    let chart_title = section_title("Your Chart");
    chart_title.set_hexpand(true);
    let node_id_label = gtk::Label::new(Some("Node ···----"));
    node_id_label.set_css_classes(&["node-id"]);
    chart_header.append(&chart_title);
    chart_header.append(&node_id_label);
    chart_box.append(&chart_header);

    chart_box.append(&section_label("Natal Aspects"));

    let natal_scroll = scrolled();
    let natal_label = aspect_label(&model.natal_text);
    natal_scroll.set_child(Some(&natal_label));
    chart_box.append(&natal_scroll);

    chart_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    chart_box.append(&section_label("Current Transits"));

    let transit_scroll = scrolled();
    let transit_label = aspect_label(&model.transit_text);
    transit_scroll.set_child(Some(&transit_label));
    chart_box.append(&transit_scroll);

    root_box.append(&chart_box);
    root_box.append(&gtk::Separator::new(gtk::Orientation::Vertical));

    // ── peers panel ───────────────────────────────────────────────────────────
    let peers_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    peers_box.set_css_classes(&["peers-panel"]);
    peers_box.set_hexpand(true);

    let peers_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    peers_header.set_css_classes(&["panel-header"]);
    let peers_title = section_title("Nearby Peers");
    peers_title.set_hexpand(true);
    let peer_count_label = gtk::Label::new(Some("Scanning for peers…"));
    peer_count_label.set_css_classes(&["peer-count"]);
    peers_header.append(&peers_title);
    peers_header.append(&peer_count_label);
    peers_box.append(&peers_header);

    let hint = gtk::Label::new(Some(
        "Peers sharing your astrological neighbourhood appear below.\n\
         Aspect glyphs show their approximate ☉ resonance with your natal chart.",
    ));
    hint.set_css_classes(&["hint-label"]);
    hint.set_wrap(true);
    hint.set_halign(gtk::Align::Start);
    hint.set_margin_start(14);
    hint.set_margin_end(14);
    hint.set_margin_top(10);
    hint.set_margin_bottom(10);
    peers_box.append(&hint);

    let peers_scrolled = gtk::ScrolledWindow::new();
    peers_scrolled.set_vexpand(true);
    peers_scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    peers_box.append(&peers_scrolled);

    root_box.append(&peers_box);

    // ── call bar (overlay at bottom of peers panel) ───────────────────────────
    let call_bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    call_bar.set_css_classes(&["call-bar"]);
    call_bar.set_margin_start(14);
    call_bar.set_margin_end(14);
    call_bar.set_margin_top(8);
    call_bar.set_margin_bottom(8);
    call_bar.set_visible(false);

    let call_status = gtk::Label::new(None);
    call_status.set_css_classes(&["call-status"]);
    call_status.set_hexpand(true);
    call_status.set_halign(gtk::Align::Start);
    call_bar.append(&call_status);

    let accept_btn = gtk::Button::with_label("Accept  ✓");
    accept_btn.set_css_classes(&["suggested-action"]);
    accept_btn.set_visible(false);
    let s = sender.clone();
    accept_btn.connect_clicked(move |_| { s.input(AppMsg::AcceptCall); });

    let hangup_btn = gtk::Button::with_label("Hang up  ✕");
    hangup_btn.set_css_classes(&["destructive-action"]);
    let s = sender.clone();
    hangup_btn.connect_clicked(move |_| { s.input(AppMsg::HangUp); });

    call_bar.append(&accept_btn);
    call_bar.append(&hangup_btn);
    peers_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    peers_box.append(&call_bar);

    (root_box, natal_label, transit_label, peer_count_label, node_id_label,
     peers_scrolled, call_bar, call_status, accept_btn, hangup_btn)
}

// ── small widget builders ─────────────────────────────────────────────────────

fn spin(min: f64, max: f64, val: f64) -> gtk::SpinButton {
    let s = gtk::SpinButton::with_range(min, max, 1.0);
    s.set_value(val);
    s.set_digits(0);
    s.set_css_classes(&["spin-field"]);
    s
}

fn entry(placeholder: &str) -> gtk::Entry {
    let e = gtk::Entry::new();
    e.set_placeholder_text(Some(placeholder));
    e.set_input_purpose(gtk::InputPurpose::Number);
    e.set_width_chars(16);
    e
}

fn section_title(t: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(t));
    l.set_css_classes(&["panel-title"]);
    l.set_halign(gtk::Align::Start);
    l
}

fn section_label(t: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(t));
    l.set_css_classes(&["section-label"]);
    l.set_halign(gtk::Align::Start);
    l.set_margin_start(14);
    l.set_margin_top(12);
    l
}

fn scrolled() -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::new();
    s.set_vexpand(true);
    s.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    s
}

fn aspect_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_css_classes(&["aspect-list"]);
    l.set_halign(gtk::Align::Start);
    l.set_valign(gtk::Align::Start);
    l.set_margin_start(14);
    l.set_margin_end(14);
    l.set_margin_top(6);
    l.set_margin_bottom(6);
    l.set_selectable(true);
    l
}

// ── text builders ─────────────────────────────────────────────────────────────

/// Generate a unique session ID from the peer's key + current nanosecond time.
/// Uses BLAKE3 so the output is well-distributed even with low-entropy inputs.
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

fn build_natal_text(chart: &Chart, store: &ZodiaStore) -> String {
    let aspects = chart.natal_aspects();
    if aspects.is_empty() {
        return "(none within default orbs)".to_string();
    }
    aspects.iter().map(|a| format_aspect_card(a, store)).collect::<Vec<_>>().join("\n\n")
}

fn build_transit_text(chart: &Chart, jdn: f64, store: &ZodiaStore) -> String {
    match chart.transits_at(jdn) {
        Err(e) => format!("(transit error: {e})"),
        Ok(ts) => {
            let mut lines: Vec<String> = ts.transit_aspects.iter()
                .map(|ta| format_transit_card(ta, store))
                .collect();
            if !ts.house_transits.is_empty() {
                lines.push(String::new());
                lines.push("― house ingresses ―".to_string());
                for ht in &ts.house_transits {
                    lines.push(format_house_transit_card(ht, store));
                }
            }
            if lines.is_empty() {
                "(no close transits today)".to_string()
            } else {
                lines.join("\n")
            }
        }
    }
}
