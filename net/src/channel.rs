//! Direct peer-to-peer channel over an iroh QUIC connection.
//!
//! Used for all post-tier-0 exchanges:
//!   - Tier 1: `Tier1Blob` exchange + X3DH key agreement
//!   - Call signaling: `ChannelMsg` over reliable QUIC streams
//!   - Audio transport: raw QUIC datagrams (unreliable, low-latency)
//!
//! Reliable messages use a length-prefix (4 bytes, big-endian u32) over fresh
//! bidirectional QUIC streams — one stream per message to isolate HoL blocking.

use crate::Tier1Blob;
use bytes::Bytes;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("connection closed")]
    Closed,
    #[error("send error: {0}")]
    Send(String),
    #[error("receive error: {0}")]
    Recv(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("message exceeds maximum size ({0} bytes)")]
    TooLarge(u32),
}

/// Maximum single reliable message size (1 MiB).
const MAX_MSG_BYTES: u32 = 1024 * 1024;

// ── interpretation sync types ─────────────────────────────────────────────────

/// A single signed community interpretation exchanged during Tier-1 sync.
///
/// `author_sig` is an ed25519 signature over `BLAKE3(interp_key || body)`,
/// verifiable against `author_pk`.  The receiver must verify before storing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterpEntry {
    /// Canonical key string, e.g. `"natal:jupiter_trine_venus"`.
    pub interp_key: String,
    pub body: String,
    pub author_pk: [u8; 32],
    /// CBOR encodes as a byte-string.  Length must be 64 on receive.
    pub author_sig: Vec<u8>,
}

// ── signaling messages ────────────────────────────────────────────────────────

/// Explicit presence state, sent over the Tier-1 channel.
///
/// `Active` is sent immediately after a channel is established so both sides
/// know the peer is present.  `Away` is sent before the app closes or idles,
/// so the remote peer marks them offline without waiting for a QUIC timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerStatus {
    Active,
    Away,
}

/// Typed messages exchanged over reliable QUIC streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChannelMsg {
    /// Tier-1 handshake: exchange exact birth data + X25519 prekeys.
    Tier1Handshake(Tier1Blob),
    /// Post-handshake: swap top community interpretations for each other's keys.
    InterpShare { entries: Vec<InterpEntry> },
    /// Explicit presence update — sent on connect (Active) and before quit (Away).
    StatusUpdate { status: PeerStatus },
    /// Caller proposes a voice session.
    CallOffer  { session_id: [u8; 32] },
    /// Callee accepts.
    CallAccept { session_id: [u8; 32] },
    /// Callee rejects.
    CallReject { session_id: [u8; 32] },
    /// Either party ends an active session.
    CallHangup { session_id: [u8; 32] },
    /// Plain-text chat message from the remote peer.
    ChatMsg { text: String },
    /// Blind-relay envelope: forward `payload` (ECIES-encrypted) to `dest`.
    ///
    /// The relay can see `dest` (needed to route) but cannot decrypt `payload`.
    RelayMsg { dest: [u8; 32], payload: Vec<u8> },
}

/// The inner message encrypted inside a `RelayMsg` payload.
///
/// CBOR-serialised then ECIES-encrypted to the destination's relay key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayPayload {
    /// The original sender's node ID (ed25519 public key bytes).
    pub from: [u8; 32],
    /// The plaintext message content.
    pub text: String,
}

// ── channel ───────────────────────────────────────────────────────────────────

/// A framed duplex channel over an iroh QUIC connection.
///
/// Clone is cheap — `Connection` is internally reference-counted.
#[derive(Clone, Debug)]
pub struct DirectChannel {
    conn: Connection,
}

impl DirectChannel {
    pub fn from_connection(conn: Connection) -> Self {
        Self { conn }
    }

    /// The underlying iroh connection — used by `zodia-av` for audio datagrams.
    pub fn connection(&self) -> Connection {
        self.conn.clone()
    }

    // ── reliable stream methods ───────────────────────────────────────────────

    /// Send a byte frame: `[ len (4B, BE) | payload ]` over a new QUIC stream.
    pub async fn send_raw(&self, bytes: &[u8]) -> Result<(), ChannelError> {
        if bytes.len() > MAX_MSG_BYTES as usize {
            return Err(ChannelError::TooLarge(bytes.len() as u32));
        }
        let (mut send, _recv): (SendStream, RecvStream) = self
            .conn
            .open_bi()
            .await
            .map_err(|e| ChannelError::Send(e.to_string()))?;

        let len_bytes = (bytes.len() as u32).to_be_bytes();
        send.write_all(&len_bytes)
            .await
            .map_err(|e| ChannelError::Send(e.to_string()))?;
        send.write_all(bytes)
            .await
            .map_err(|e| ChannelError::Send(e.to_string()))?;
        send.finish()
            .map_err(|e| ChannelError::Send(e.to_string()))?;
        Ok(())
    }

    /// Receive a length-prefixed frame from the next incoming QUIC stream.
    pub async fn recv_raw(&self) -> Result<Vec<u8>, ChannelError> {
        let (_send, mut recv): (SendStream, RecvStream) = self
            .conn
            .accept_bi()
            .await
            .map_err(|e| ChannelError::Recv(e.to_string()))?;

        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| ChannelError::Recv(e.to_string()))?;

        let len = u32::from_be_bytes(len_buf);
        if len > MAX_MSG_BYTES {
            return Err(ChannelError::TooLarge(len));
        }

        let mut buf = vec![0u8; len as usize];
        recv.read_exact(&mut buf)
            .await
            .map_err(|e| ChannelError::Recv(e.to_string()))?;
        Ok(buf)
    }

    /// Send a typed `ChannelMsg` over a new reliable stream.
    pub async fn send_msg(&self, msg: &ChannelMsg) -> Result<(), ChannelError> {
        self.send_raw(&cbor_encode(msg)).await
    }

    /// Receive a typed `ChannelMsg` from the next incoming stream.
    pub async fn recv_msg(&self) -> Result<ChannelMsg, ChannelError> {
        let bytes = self.recv_raw().await?;
        ciborium::from_reader(bytes.as_slice())
            .map_err(|e| ChannelError::Decode(e.to_string()))
    }

    // ── datagram methods (unreliable, low-latency — audio transport) ──────────

    /// Send a QUIC datagram (fire-and-forget, no ordering guarantee).
    pub fn send_datagram(&self, data: Bytes) -> Result<(), ChannelError> {
        self.conn
            .send_datagram(data)
            .map_err(|e| ChannelError::Send(e.to_string()))
    }

    /// Receive the next incoming QUIC datagram.
    pub async fn recv_datagram(&self) -> Result<Bytes, ChannelError> {
        self.conn
            .read_datagram()
            .await
            .map_err(|e| ChannelError::Recv(e.to_string()))
    }

    // ── interpretation sync ───────────────────────────────────────────────────

    /// Mutual interpretation exchange — call concurrently on both sides after
    /// the Tier-1 handshake completes.
    ///
    /// Each peer sends their top signed community entries for the other's
    /// relevant aspect keys.  Returns the entries received from the remote peer.
    pub async fn exchange_interps(
        &self,
        ours: &[InterpEntry],
    ) -> Result<Vec<InterpEntry>, ChannelError> {
        let msg = ChannelMsg::InterpShare { entries: ours.to_vec() };
        let encoded = cbor_encode(&msg);
        let (send_res, recv_res) = tokio::join!(self.send_raw(&encoded), self.recv_raw());
        send_res?;
        let bytes = recv_res?;
        match ciborium::from_reader(bytes.as_slice())
            .map_err(|e| ChannelError::Decode(e.to_string()))?
        {
            ChannelMsg::InterpShare { entries } => Ok(entries),
            _ => Err(ChannelError::Decode("expected InterpShare".into())),
        }
    }

    // ── tier-1 handshake ──────────────────────────────────────────────────────

    /// Mutual Tier-1 blob exchange.
    ///
    /// Both sides call this concurrently.  QUIC bidirectional streams let
    /// simultaneous send/receive proceed without deadlocking.
    pub async fn exchange_tier1(&self, ours: &Tier1Blob) -> Result<Tier1Blob, ChannelError> {
        let encoded = cbor_encode(ours);
        let (send_res, recv_res) = tokio::join!(self.send_raw(&encoded), self.recv_raw());
        send_res?;
        let bytes = recv_res?;
        ciborium::from_reader(bytes.as_slice())
            .map_err(|e| ChannelError::Decode(e.to_string()))
    }
}

fn cbor_encode<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf)
        .expect("CBOR encoding is infallible for in-memory writes");
    buf
}
