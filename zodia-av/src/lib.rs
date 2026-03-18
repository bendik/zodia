//! WebRTC audio/video sessions for Zodia via `webrtc-rs`.
//!
//! Signaling (offer/answer/ICE) flows through the existing noise-encrypted
//! p2panda channel — no STUN server ever sees peer identities.
//!
//! Sessions open only after both peers have reached Tier 1.

use serde::{Deserialize, Serialize};
use zodia_net::PeerId;

/// State of a single AV session with one peer.
#[derive(Debug)]
pub struct AvSession {
    pub peer_id: PeerId,
    // rtc: RTCPeerConnection  -- added with webrtc-rs dep
    pub state: AvState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AvState {
    /// Offer/answer exchange in progress over the noise channel.
    Signaling,
    /// ICE negotiation complete, media flowing.
    Connected { video: bool, audio: bool },
    /// Session terminated by either party.
    Ended,
}

/// WebRTC signaling messages passed through the noise channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignalingMsg {
    Offer  { sdp: String },
    Answer { sdp: String },
    Ice    { candidate: String, sdp_mid: Option<String>, sdp_mline_index: Option<u16> },
}
