//! Zodia AV — Opus voice calls over iroh QUIC datagrams.
//!
//! # Pipeline
//!
//! ```text
//!  cpal capture callback
//!       └─ ringbuf Producer<f32>
//!            └─ std::thread "zodia-encoder"
//!                 └─ tokio mpsc Sender<Bytes>
//!                      └─ tokio task "quic-send"
//!                           └─ conn.send_datagram(...)
//!
//!  conn.read_datagram()
//!       └─ tokio task "quic-recv"
//!            └─ tokio mpsc Sender<Bytes>
//!                 └─ std::thread "zodia-decoder"
//!                      └─ ringbuf Producer<f32>
//!                           └─ cpal output callback
//! ```
//!
//! # Usage
//!
//! ```ignore
//! let session = AudioSession::start(channel).await?;
//! // ... keep `session` alive for the duration of the call ...
//! drop(session); // or let it go out of scope to hang up
//! ```

pub mod codec;
mod capture;
mod playback;
mod transport;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ringbuf::HeapRb;
use thiserror::Error;
use zodia_net::channel::DirectChannel;

/// 4× frame capacity in the capture ring buffer (~80 ms of headroom).
const CAPTURE_BUF_FRAMES: usize = 4;
/// 8× frame capacity in the playback ring buffer (~160 ms of jitter headroom).
const PLAYBACK_BUF_FRAMES: usize = 8;

#[derive(Debug, Error)]
pub enum AvError {
    #[error("capture: {0}")]
    Capture(#[from] capture::CaptureError),
    #[error("playback: {0}")]
    Playback(#[from] playback::PlaybackError),
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
}

/// A live audio session with one peer.
///
/// Drop to end the call — the codec threads and QUIC tasks shut down
/// automatically when the `shutdown` flag is set and their channels close.
///
/// Audio streams are `Option` — if no suitable device is found, the call
/// continues without audio (the QUIC transport and signaling still work).
pub struct AudioSession {
    /// Keeps the capture stream alive (drop = mic off).  `None` = no mic.
    _capture: Option<cpal::Stream>,
    /// Keeps the playback stream alive (drop = speaker off).  `None` = muted.
    _playback: Option<cpal::Stream>,
    /// Signals codec threads to exit.
    shutdown: Arc<AtomicBool>,
}

impl AudioSession {
    /// Start capture, transport, and playback using the given `DirectChannel`.
    ///
    /// The channel's underlying QUIC connection is used for audio datagrams.
    /// Signaling (CallOffer/Accept/Hangup) must be done separately via
    /// `channel.send_msg()` before calling this.
    pub async fn start(channel: &DirectChannel) -> Result<Self, AvError> {
        let shutdown = Arc::new(AtomicBool::new(false));

        let cap_samples = codec::FRAME_SAMPLES * CAPTURE_BUF_FRAMES;
        let play_samples = codec::FRAME_SAMPLES * PLAYBACK_BUF_FRAMES;

        let (cap_prod, cap_cons) = HeapRb::<f32>::new(cap_samples).split();
        let (play_prod, play_cons) = HeapRb::<f32>::new(play_samples).split();

        let _capture = match capture::start_capture(cap_prod) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("microphone unavailable, call will be muted: {e}");
                None
            }
        };

        let _playback = match playback::start_playback(play_cons) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("speaker unavailable, call will be silent: {e}");
                None
            }
        };

        transport::start_transport(
            channel.connection(),
            Arc::clone(&shutdown),
            cap_cons,
            play_prod,
        );

        Ok(AudioSession { _capture, _playback, shutdown })
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
