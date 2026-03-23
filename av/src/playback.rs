//! Speaker playback via cpal.
//!
//! Pulls decoded f32 mono samples at 48 kHz from the ring-buffer consumer.
//! If the buffer underruns, silence is written; the decoder thread will have
//! already generated PLC frames to backfill the gap where possible.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleRate, StreamConfig};
use ringbuf::HeapConsumer;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlaybackError {
    #[error("no default output device")]
    NoDevice,
    #[error("cpal build stream: {0}")]
    Build(String),
    #[error("cpal play stream: {0}")]
    Play(String),
}

/// Starts a cpal output stream.  Returns the stream (must be kept alive).
pub fn start_playback(mut cons: HeapConsumer<f32>) -> Result<cpal::Stream, PlaybackError> {
    let host = cpal::default_host();
    let device = find_output_device(&host).ok_or(PlaybackError::NoDevice)?;

    let config = StreamConfig {
        channels: 1,
        sample_rate: SampleRate(crate::codec::SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    let stream = device
        .build_output_stream::<f32, _, _>(
            &config,
            move |output: &mut [f32], _info| {
                let n = cons.pop_slice(output);
                // Fill any underrun with silence.
                output[n..].fill(0.0);
            },
            |err| tracing::error!("playback stream error: {err}"),
            None,
        )
        .map_err(|e| PlaybackError::Build(e.to_string()))?;

    stream.play().map_err(|e| PlaybackError::Play(e.to_string()))?;
    Ok(stream)
}

/// Try the default output device first; if it has no supported configs, fall
/// back to enumerating all output devices and return the first usable one.
fn find_output_device(host: &cpal::Host) -> Option<cpal::Device> {
    if let Some(dev) = host.default_output_device() {
        if dev.supported_output_configs().map(|mut c| c.next().is_some()).unwrap_or(false) {
            return Some(dev);
        }
        tracing::warn!("default output device has no supported configs, enumerating");
    }
    host.output_devices().ok()?.next()
}
