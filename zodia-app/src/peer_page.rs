//! Connected-peer navigation page.
//!
//! Pushed onto the app's `adw::NavigationView` when a Tier-1 exchange
//! completes.  Shows:
//!   - A header bar with the peer's solar sign glyph, truncated node ID,
//!     and a call button.
//!   - An `AspectView` (adaptive NavigationSplitView) populated with
//!     cross-chart synastry aspects computed from the peer's exact birth data.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita as adw;
use libadwaita::gtk;
use libadwaita::prelude::*;
use relm4::AsyncComponentSender;
use zodia_core::{compute_positions, compute_synastry, Chart};
use zodia_net::{PeerId, Tier1Blob};
use zodia_store::ZodiaStore;

use crate::app::{AppModel, AppMsg};
use crate::aspect_list::synastry_items;
use crate::aspect_view::AspectView;
use crate::util::sign_glyph;

/// Build the `adw::NavigationPage` for a connected peer.
///
/// Synastry is computed here from `their_blob.birth` and `our_chart.positions`.
/// If ephemeris fails for the peer's JDN, an empty aspect list is shown.
pub fn build_peer_page(
    peer_id: &PeerId,
    their_blob: &Tier1Blob,
    our_chart: &Chart,
    store: Rc<RefCell<ZodiaStore>>,
    author_pk: [u8; 32],
    sender: &AsyncComponentSender<AppModel>,
) -> adw::NavigationPage {
    let peer_hex = hex::encode_upper(&peer_id.0[..4]);

    // ── synastry computation ──────────────────────────────────────────────────

    let synastry = match compute_positions(their_blob.birth.jdn) {
        Ok(their_pos) => compute_synastry(&our_chart.positions, &their_pos),
        Err(e) => {
            tracing::warn!(peer = %peer_hex, "synastry computation failed: {e}");
            vec![]
        }
    };
    let items = synastry_items(&synastry);

    // ── layout ────────────────────────────────────────────────────────────────

    let toolbar_view = adw::ToolbarView::new();

    // Header bar
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let their_solar_month = zodia_core::solar_month(their_blob.birth.jdn);
    let glyph = sign_glyph(their_solar_month);
    let title_lbl = gtk::Label::new(Some(&format!("{glyph}  ···{peer_hex}")));
    title_lbl.add_css_class("title");
    header.set_title_widget(Some(&title_lbl));

    let call_btn = gtk::Button::from_icon_name("call-start-symbolic");
    call_btn.add_css_class("suggested-action");
    call_btn.add_css_class("circular");
    call_btn.set_tooltip_text(Some("Start voice call"));

    let pid = peer_id.clone();
    let s = sender.clone();
    call_btn.connect_clicked(move |_| s.input(AppMsg::CallPeer(pid.clone())));
    header.pack_end(&call_btn);

    toolbar_view.add_top_bar(&header);

    // Synastry aspect view
    let av = AspectView::new(items, store, author_pk);
    av.widget().set_vexpand(true);
    toolbar_view.set_content(Some(av.widget()));

    adw::NavigationPage::new(&toolbar_view, &format!("···{peer_hex}"))
}
