//! Opus encoder/decoder wrappers with `unsafe impl Send`.
//!
//! libopus is **per-instance** thread-safe: a single instance must only be
//! used from one thread at a time, but separate instances are independent.
//! We move each instance into its own dedicated `std::thread` and never share
//! it across threads, which makes the `unsafe impl Send` sound.

use opus::{Application, Channels};

/// 20 ms frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;
pub const SAMPLE_RATE: u32 = 48_000;
pub const OPUS_BITRATE_BPS: i32 = 24_000;

// ── encoder ───────────────────────────────────────────────────────────────────

pub struct Encoder(opus::Encoder);

/// SAFETY: libopus encoder instances are not shared across threads;
/// each lives exclusively on the std::thread that created it.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Self, opus::Error> {
        let mut enc = opus::Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)?;
        enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE_BPS))?;
        Ok(Encoder(enc))
    }

    /// Encode one 960-sample frame.  Returns the number of output bytes.
    pub fn encode_float(&mut self, input: &[f32], output: &mut [u8]) -> Result<usize, opus::Error> {
        self.0.encode_float(input, output)
    }
}

// ── decoder ───────────────────────────────────────────────────────────────────

pub struct Decoder(opus::Decoder);

/// SAFETY: same reasoning as `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, opus::Error> {
        Ok(Decoder(opus::Decoder::new(SAMPLE_RATE, Channels::Mono)?))
    }

    /// Decode one Opus packet into `output`.  Pass `input = &[]` for PLC.
    pub fn decode_float(
        &mut self,
        input: &[u8],
        output: &mut [f32],
        fec: bool,
    ) -> Result<usize, opus::Error> {
        self.0.decode_float(input, output, fec)
    }
}
