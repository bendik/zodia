//! Direct peer-to-peer channel over an iroh QUIC connection.
//!
//! Used for all post-tier-0 exchanges:
//!   - Tier 1: `Tier1Blob` exchange + X3DH key agreement
//!   - Tier 2: WebRTC offer/answer/ICE (signaling)
//!   - Tier 3: in-session plaintext messages
//!
//! Each message is length-prefixed (4 bytes, big-endian u32) over fresh
//! bidirectional QUIC streams.  Opening a new stream per message keeps
//! head-of-line blocking isolated to individual messages.

use crate::Tier1Blob;
use iroh::endpoint::{Connection, RecvStream, SendStream};
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

/// Maximum single message size (1 MiB).
const MAX_MSG_BYTES: u32 = 1024 * 1024;

/// A framed duplex channel over an iroh QUIC connection.
pub struct DirectChannel {
    conn: Connection,
}

impl DirectChannel {
    pub(crate) fn from_connection(conn: Connection) -> Self {
        Self { conn }
    }

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
