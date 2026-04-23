//! Decoder abstraction and backend registry.

#[cfg(feature = "opus")]
pub mod opus;

#[cfg(all(target_arch = "wasm32", feature = "webcodecs"))]
pub mod webcodecs;

use crate::{AudioError, DecodeError, PtsNs, codec::AudioCodecKind};

/// A block of decoded PCM samples.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// Interleaved float32 PCM samples in `[−1.0, 1.0]`, channel-major.
    ///
    /// Length is always `channels * frame_count`.
    pub samples: Vec<f32>,

    /// Number of interleaved channels.
    pub channels: u16,

    /// Sample rate of the decoded PCM.
    pub sample_rate: u32,

    /// Presentation timestamp of the first sample, in nanoseconds.
    pub pts_ns: PtsNs,
}

impl DecodedAudio {
    /// Number of frames (samples per channel) in this block.
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    /// Total duration of this block, in nanoseconds.
    pub fn duration_ns(&self) -> i64 {
        if self.sample_rate == 0 {
            0
        } else {
            i64::try_from(self.frame_count()).unwrap_or(i64::MAX) * 1_000_000_000
                / i64::from(self.sample_rate)
        }
    }
}

/// Backend-agnostic audio decoder.
///
/// Implementors must honor the following contract:
///
/// * `decode` is given a single encoded segment (Opus packet, FLAC frame,
///   etc.) and returns the PCM it produces.
/// * `reset` must fully discard any internal decoder state. It is called
///   on seek, discontinuity, or codec reconfiguration.
/// * Implementations do not buffer output across calls: the caller is
///   responsible for ring-buffering.
pub trait AudioDecoder: Send {
    /// Decode one encoded segment.
    fn decode(&mut self, chunk: &[u8], pts_ns: PtsNs) -> Result<DecodedAudio, DecodeError>;

    /// Discard all internal decoder state.
    fn reset(&mut self);

    /// Codec this decoder handles.
    fn codec(&self) -> AudioCodecKind;
}

/// Descriptor used to instantiate a decoder.
#[derive(Debug, Clone, Copy)]
pub struct DecoderConfig {
    /// Codec the decoder should handle.
    pub codec: AudioCodecKind,

    /// Sample rate requested of the decoder, in Hz.
    pub sample_rate: u32,

    /// Channel count.
    pub channels: u16,
}

/// Construct a decoder for the given codec / stream config on the current
/// platform, picking the first available backend.
pub fn make_decoder(config: DecoderConfig) -> Result<Box<dyn AudioDecoder>, AudioError> {
    match config.codec {
        #[cfg(feature = "opus")]
        AudioCodecKind::Opus => Ok(Box::new(opus::OpusDecoder::new(
            config.sample_rate,
            config.channels,
        )?)),

        #[cfg(not(feature = "opus"))]
        AudioCodecKind::Opus => Err(AudioError::UnsupportedCodec("opus")),

        AudioCodecKind::Flac => Err(AudioError::UnsupportedCodec("flac")),
    }
}
