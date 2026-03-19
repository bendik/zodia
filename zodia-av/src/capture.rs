//! Microphone capture via cpal.
//!
//! Starts a cpal input stream that pushes f32 mono samples at 48 kHz
//! into the supplied ring-buffer producer.  Samples outside the buffer
//! capacity are dropped (encoder will request PLC if it starves).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleRate, StreamConfig};
use ringbuf::HeapProducer;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("no default input device")]
    NoDevice,
    #[error("cpal build stream: {0}")]
    Build(String),
    #[error("cpal play stream: {0}")]
    Play(String),
}

/// Starts a cpal input stream.  Returns the stream (must be kept alive).
pub fn start_capture(mut prod: HeapProducer<f32>) -> Result<cpal::Stream, CaptureError> {
    let host = cpal::default_host();
    let device = find_input_device(&host).ok_or(CaptureError::NoDevice)?;

    let config = StreamConfig {
        channels: 1,
        sample_rate: SampleRate(crate::codec::SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    let stream = device
        .build_input_stream::<f32, _, _>(
            &config,
            move |data: &[f32], _info| {
                // Push as many samples as fit; silently drop the rest.
                prod.push_slice(data);
            },
            |err| tracing::error!("capture stream error: {err}"),
            None,
        )
        .map_err(|e| CaptureError::Build(e.to_string()))?;

    stream.play().map_err(|e| CaptureError::Play(e.to_string()))?;
    Ok(stream)
}

/// Try the default input device first; if it has no supported configs, fall
/// back to enumerating all input devices and return the first usable one.
fn find_input_device(host: &cpal::Host) -> Option<cpal::Device> {
    if let Some(dev) = host.default_input_device() {
        if dev.supported_input_configs().map(|mut c| c.next().is_some()).unwrap_or(false) {
            return Some(dev);
        }
        tracing::warn!("default input device has no supported configs, enumerating");
    }
    host.input_devices().ok()?.next()
}
