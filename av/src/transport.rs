//! Encode → QUIC datagram → decode pipeline.
//!
//! Two std::threads handle codec work (Opus is !Send; each instance lives
//! entirely on its own thread).  Two tokio tasks shuttle Bytes across the
//! QUIC connection.
//!
//! Wire format: `[ seq: u32 BE (4 bytes) | Opus packet ]`
//!
//! The decoder thread performs simple gap-based PLC: if `seq` jumps by more
//! than 1, it generates up to 3 concealment frames before decoding the
//! received frame.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use iroh::endpoint::Connection;
use ringbuf::{HeapConsumer, HeapProducer};

use crate::codec::{Decoder, Encoder, FRAME_SAMPLES};

const MAX_OPUS_PACKET: usize = 4_000;
/// How many decoded samples to poll per iteration when the buffer is short.
const POLL_CHUNK: usize = 256;

/// Spawn all four transport actors (encoder thread, send task, recv task,
/// decoder thread) around the given QUIC connection.
///
/// `cap_cons` — consumer side of the capture ring buffer (encoder reads here)
/// `play_prod` — producer side of the playback ring buffer (decoder writes here)
pub fn start_transport(
    conn: Connection,
    shutdown: Arc<AtomicBool>,
    mut cap_cons: HeapConsumer<f32>,
    mut play_prod: HeapProducer<f32>,
) {
    // Encoder thread → QUIC send task
    let (enc_tx, enc_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
    // QUIC recv task → decoder thread
    let (dec_tx, dec_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);

    // ── encoder std::thread ───────────────────────────────────────────────────
    let enc_shutdown = shutdown.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("zodia-encoder".into())
        .spawn(move || {
            let mut enc = match Encoder::new() {
                Ok(e) => e,
                Err(err) => {
                    tracing::error!("opus encoder init: {err}");
                    return;
                }
            };
            let mut acc = Vec::<f32>::with_capacity(FRAME_SAMPLES * 4);
            let mut tmp = vec![0f32; POLL_CHUNK];
            let mut opus_buf = vec![0u8; MAX_OPUS_PACKET];
            let mut seq: u32 = 0;

            while !enc_shutdown.load(Relaxed) {
                let n = cap_cons.pop_slice(&mut tmp);
                if n > 0 {
                    acc.extend_from_slice(&tmp[..n]);
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }

                while acc.len() >= FRAME_SAMPLES {
                    let frame: Vec<f32> = acc.drain(..FRAME_SAMPLES).collect();
                    match enc.encode_float(&frame, &mut opus_buf) {
                        Ok(n_bytes) => {
                            let mut pkt = BytesMut::with_capacity(4 + n_bytes);
                            pkt.put_u32(seq);
                            pkt.put_slice(&opus_buf[..n_bytes]);
                            seq = seq.wrapping_add(1);
                            // blocking_send: park the encoder thread if the
                            // tokio task is behind (back-pressure).
                            if enc_tx.blocking_send(pkt.freeze()).is_err() {
                                return; // send task dropped — session ended
                            }
                        }
                        Err(e) => tracing::warn!("opus encode: {e}"),
                    }
                }
            }
        }) {
        tracing::error!("failed to spawn encoder thread: {e}");
    }

    // ── QUIC datagram send task ───────────────────────────────────────────────
    let conn_send = conn.clone();
    tokio::spawn(async move {
        let mut enc_rx = enc_rx;
        while let Some(pkt) = enc_rx.recv().await {
            if let Err(e) = conn_send.send_datagram(pkt) {
                tracing::debug!("datagram send: {e}");
                break;
            }
        }
    });

    // ── QUIC datagram recv task ───────────────────────────────────────────────
    tokio::spawn(async move {
        loop {
            match conn.read_datagram().await {
                Ok(bytes) => {
                    if dec_tx.send(bytes).await.is_err() {
                        break; // decoder thread dropped — session ended
                    }
                }
                Err(e) => {
                    tracing::debug!("datagram recv: {e}");
                    break;
                }
            }
        }
    });

    // ── decoder std::thread ───────────────────────────────────────────────────
    let dec_shutdown = shutdown.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("zodia-decoder".into())
        .spawn(move || {
            let mut dec = match Decoder::new() {
                Ok(d) => d,
                Err(err) => {
                    tracing::error!("opus decoder init: {err}");
                    return;
                }
            };
            let mut pcm = vec![0f32; FRAME_SAMPLES];
            let mut last_seq: u32 = 0;
            let mut first = true;
            let mut dec_rx = dec_rx;

            while !dec_shutdown.load(Relaxed) {
                let bytes = match dec_rx.blocking_recv() {
                    Some(b) => b,
                    None => break, // send task dropped
                };

                if bytes.len() < 4 {
                    continue;
                }
                let seq = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let opus_data = &bytes[4..];

                // PLC for missing frames (cap at 3 to avoid runaway fill).
                if !first {
                    let missed = seq.wrapping_sub(last_seq).wrapping_sub(1).min(3) as usize;
                    for _ in 0..missed {
                        if let Ok(n) = dec.decode_float(&[], &mut pcm, true) {
                            play_prod.push_slice(&pcm[..n]);
                        }
                    }
                }
                first = false;
                last_seq = seq;

                match dec.decode_float(opus_data, &mut pcm, false) {
                    Ok(n) => { play_prod.push_slice(&pcm[..n]); }
                    Err(e) => tracing::warn!("opus decode: {e}"),
                }
            }
        }) {
        tracing::error!("failed to spawn decoder thread: {e}");
    }
}
