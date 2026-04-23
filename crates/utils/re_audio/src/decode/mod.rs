//! Decoder abstraction and backend registry.

use std::collections::HashMap;

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

/// Factory used by [`DecoderRegistry`] to construct a decoder.
pub type DecoderFactory = fn(DecoderConfig) -> Result<Box<dyn AudioDecoder>, AudioError>;

/// Runtime decoder registry.
///
/// The public audio schema records codec ids, but playback support is decided
/// by this registry. Viewer and tool builds can therefore advertise metadata,
/// waveform summaries, annotations, and seek indexes for codecs they cannot
/// decode locally.
#[derive(Clone, Default)]
pub struct DecoderRegistry {
    factories: HashMap<AudioCodecKind, DecoderFactory>,
}

impl DecoderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create the registry for the codecs supported by this build.
    pub fn platform_default() -> Self {
        let mut registry = Self::new();
        register_platform_decoders(&mut registry);
        registry
    }

    /// Register or replace the factory for `codec`.
    pub fn register(&mut self, codec: AudioCodecKind, factory: DecoderFactory) {
        self.factories.insert(codec, factory);
    }

    /// Returns `true` if this registry can instantiate `codec`.
    pub fn supports(&self, codec: AudioCodecKind) -> bool {
        self.factories.contains_key(&codec)
    }

    /// Iterate codec ids supported by this registry.
    pub fn supported_codecs(&self) -> impl Iterator<Item = AudioCodecKind> + '_ {
        self.factories.keys().copied()
    }

    /// Construct a decoder for the given codec / stream config.
    pub fn make_decoder(&self, config: DecoderConfig) -> Result<Box<dyn AudioDecoder>, AudioError> {
        let Some(factory) = self.factories.get(&config.codec) else {
            return Err(AudioError::UnsupportedCodec(config.codec.display_name()));
        };

        factory(config)
    }
}

/// Construct a decoder using [`DecoderRegistry::platform_default`].
pub fn make_decoder(config: DecoderConfig) -> Result<Box<dyn AudioDecoder>, AudioError> {
    DecoderRegistry::platform_default().make_decoder(config)
}

#[cfg(feature = "opus")]
fn make_opus_decoder(config: DecoderConfig) -> Result<Box<dyn AudioDecoder>, AudioError> {
    Ok(Box::new(opus::OpusDecoder::new(
        config.sample_rate,
        config.channels,
    )?))
}

#[cfg(feature = "opus")]
fn register_platform_decoders(registry: &mut DecoderRegistry) {
    registry.register(AudioCodecKind::Opus, make_opus_decoder);
}

#[cfg(not(feature = "opus"))]
fn register_platform_decoders(_registry: &mut DecoderRegistry) {}
