//! Stargazer state machine — data types behind the peer list UI.

use zodia_net::{AnnounceBlob, ConsentBlob, DirectChannel, PeerId};

/// Full lifecycle of a known peer, from first gossip sighting to connected.
#[derive(Debug)]
pub enum StargazerState {
    /// Gossip announce seen; no direct contact initiated.
    Discovered,
    /// We initiated consent exchange — persisted in pending.tsv, retried on
    /// each re-discovery until the exchange succeeds or the user removes them.
    OutgoingPending,
    /// They opened a channel to us; held waiting for our approval.
    IncomingPending { channel: DirectChannel },
    /// Tier-1 complete — mutual chart exchange done.
    Connected { birth: ConsentBlob },
}

/// A single known stargazer, in any state.
#[derive(Debug)]
pub struct Stargazer {
    pub peer_id:             PeerId,
    pub solar_month:         u8,
    pub geohash_prefix:      String,
    /// Pre-computed approximate aspect glyph strings against our natal chart.
    pub approximate_aspects: Vec<String>,
    pub state:               StargazerState,
}

impl Stargazer {
    /// Construct a freshly-discovered (Tier-0 only) stargazer.
    pub fn discovered(peer_id: PeerId, blob: &AnnounceBlob, approx: Vec<String>) -> Self {
        Self {
            peer_id,
            solar_month:         blob.solar_month,
            geohash_prefix:      blob.geohash_prefix.clone(),
            approximate_aspects: approx,
            state:               StargazerState::Discovered,
        }
    }

    /// Construct a placeholder for a peer loaded from pending.tsv before their
    /// announce blob has arrived.  Announce fields are filled in on PeerDiscovered.
    pub fn outgoing_pending(peer_id: PeerId) -> Self {
        Self {
            peer_id,
            solar_month:         0,
            geohash_prefix:      String::new(),
            approximate_aspects: Vec::new(),
            state:               StargazerState::OutgoingPending,
        }
    }

    /// Sort key used to order rows in the sidebar.
    ///
    /// Lower = higher in the list.
    pub fn sidebar_sort_key(&self, has_channel: bool) -> u8 {
        match &self.state {
            StargazerState::Connected { .. } if has_channel  => 0, // online
            StargazerState::IncomingPending { .. }           => 1, // needs action
            StargazerState::OutgoingPending                  => 2, // seeking
            StargazerState::Connected { .. }                 => 3, // offline
            StargazerState::Discovered                       => 4, // not in sidebar
        }
    }
}
