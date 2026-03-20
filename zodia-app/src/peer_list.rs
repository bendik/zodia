//! Discovered peer data — the plain structs behind the peer list UI.
//!
//! The actual GTK widgets are built dynamically in `app::update_view`
//! rather than through the relm4 factory machinery, so this module is
//! intentionally light — just the data types.

use zodia_net::{PeerId, Tier0Blob};

/// A single peer seen on the gossip swarm (Tier-0 announce received).
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub peer_id: PeerId,
    pub solar_month: u8,
    pub geohash_prefix: String,
    /// Pre-computed approximate aspect glyph strings against our natal chart.
    pub approximate_aspects: Vec<String>,
}

impl DiscoveredPeer {
    pub fn from_blob(peer_id: PeerId, blob: &Tier0Blob, approx: Vec<String>) -> Self {
        Self {
            peer_id,
            solar_month: blob.solar_month,
            geohash_prefix: blob.geohash_prefix.clone(),
            approximate_aspects: approx,
        }
    }
}
