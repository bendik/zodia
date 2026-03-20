//! Connected-peer navigation page.
//!
//! Pushed onto the app's `adw::NavigationView` when a Tier-1 exchange
//! completes.  Shows two tabs:
//!   - **Their Chart** — peer's planet placements + their natal aspects.
//!   - **Synastry**    — cross-chart aspects between the two of you.
//!
//! A call button lives in the shared HeaderBar.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::AsyncComponentSender;
use zodia_core::{Chart, compute_positions, compute_synastry};
use zodia_crypto::IdentityKeypair;
use zodia_net::{PeerId, Tier1Blob};
use zodia_store::ZodiaStore;

use crate::app::{AppModel, AppMsg};
use crate::aspect_list::{natal_items, synastry_items};
use crate::aspect_view::AspectView;
use crate::util::sign_glyph;

/// Build the `adw::NavigationPage` for a connected peer.
///
/// Contains two `AspectView` tabs: their natal chart and your synastry.
#[allow(deprecated)] // ViewSwitcherTitle deprecated in ADW 1.4
pub fn build_peer_page(
    peer_id: &PeerId,
    their_blob: &Tier1Blob,
    our_chart: &Chart,
    store: Rc<RefCell<ZodiaStore>>,
    identity: Rc<IdentityKeypair>,
    sender: &AsyncComponentSender<AppModel>,
) -> adw::NavigationPage {
    let peer_hex = hex::encode_upper(&peer_id.0[..4]);

    // ── compute their chart + synastry ────────────────────────────────────────

    let their_chart = Chart::compute(their_blob.birth.clone()).ok();

    let synastry = match compute_positions(their_blob.birth.jdn) {
        Ok(their_pos) => compute_synastry(&our_chart.positions, &their_pos),
        Err(e) => {
            tracing::warn!(peer = %peer_hex, "synastry computation failed: {e}");
            vec![]
        }
    };

    // ── view stack ────────────────────────────────────────────────────────────

    let view_stack = adw::ViewStack::new();
    view_stack.set_vexpand(true);

    // Their Chart tab
    let their_av = match &their_chart {
        Some(chart) => AspectView::natal(
            natal_items(&chart.natal_aspects()),
            chart,
            Rc::clone(&store),
            Rc::clone(&identity),
        ),
        None => AspectView::new(vec![], Rc::clone(&store), Rc::clone(&identity)),
    };
    their_av.widget().set_vexpand(true);
    let their_page = view_stack.add_titled(their_av.widget(), Some("their"), "Their Chart");
    their_page.set_icon_name(Some("weather-clear-symbolic"));

    // Synastry tab
    let syn_av = AspectView::new(synastry_items(&synastry), Rc::clone(&store), Rc::clone(&identity));
    syn_av.widget().set_vexpand(true);
    let syn_page = view_stack.add_titled(syn_av.widget(), Some("synastry"), "Synastry");
    syn_page.set_icon_name(Some("people-meet-symbolic"));

    let _ = (their_page, syn_page);

    // ── toolbar view ──────────────────────────────────────────────────────────

    let toolbar_view = adw::ToolbarView::new();

    // Header bar
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let their_solar_month = zodia_core::solar_month(their_blob.birth.jdn);
    let glyph = sign_glyph(their_solar_month);

    let switcher_title = adw::ViewSwitcherTitle::new();
    switcher_title.set_stack(Some(&view_stack));
    switcher_title.set_title(&format!("{glyph}  ···{peer_hex}"));
    header.set_title_widget(Some(&switcher_title));

    let call_btn = gtk::Button::from_icon_name("call-start-symbolic");
    call_btn.add_css_class("suggested-action");
    call_btn.add_css_class("circular");
    call_btn.set_tooltip_text(Some("Start voice call"));

    let pid = peer_id.clone();
    let s = sender.clone();
    call_btn.connect_clicked(move |_| s.input(AppMsg::CallPeer(pid.clone())));
    header.pack_end(&call_btn);

    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&view_stack));

    // Bottom switcher bar (appears when window is too narrow for header tabs)
    let switcher_bar = adw::ViewSwitcherBar::new();
    switcher_bar.set_stack(Some(&view_stack));
    switcher_title
        .bind_property("title-visible", &switcher_bar, "reveal")
        .sync_create()
        .build();
    toolbar_view.add_bottom_bar(&switcher_bar);

    adw::NavigationPage::new(&toolbar_view, &format!("···{peer_hex}"))
}
