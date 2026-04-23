//! Native Opus decoder built on top of libopus via the `opus` crate.

use opus::{Channels, Decoder};

use crate::{
    DecodeError, PtsNs,
    codec::AudioCodecKind,
    decode::{AudioDecoder, DecodedAudio},
    error::AudioError,
};

/// Maximum PCM buffer we need per decode call.
///
/// Opus packets are at most 120 ms of audio; at 48 kHz stereo this is
/// `48000 * 0.12 * 2 = 11520` samples. We round up for safety.
const MAX_FRAME_SAMPLES: usize = 11_520 * 4;

/// Opus decoder wrapping libopus.
pub struct OpusDecoder {
    inner: Decoder,
    sample_rate: u32,
    channels: u16,
    scratch: Vec<f32>,
}

impl OpusDecoder {
    /// Create a decoder for the requested output rate and channel count.
    ///
    /// libopus only natively supports a fixed set of rates
    /// (8/12/16/24/48 kHz). Other rates produce an `UnsupportedCodec` error
    /// and the caller is expected to follow up with a resampler.
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, AudioError> {
        let channels_enum = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => {
                return Err(AudioError::ChannelMismatch {
                    got: channels,
                    expected: 2,
                });
            }
        };

        // libopus only speaks to these rates. For anything else we decode at
        // 48 kHz and let the resampler stage downsample.
        let decoder_rate = match sample_rate {
            8_000 | 12_000 | 16_000 | 24_000 | 48_000 => sample_rate,
            _ => 48_000,
        };

        let inner = Decoder::new(decoder_rate, channels_enum)
            .map_err(|e| AudioError::Decode(DecodeError::Backend(format!("{e}"))))?;

        Ok(Self {
            inner,
            sample_rate: decoder_rate,
            channels,
            scratch: vec![0.0; MAX_FRAME_SAMPLES],
        })
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, chunk: &[u8], pts_ns: PtsNs) -> Result<DecodedAudio, DecodeError> {
        if chunk.is_empty() {
            return Err(DecodeError::BadChunk);
        }

        let frames = self
            .inner
            .decode_float(chunk, &mut self.scratch, false)
            .map_err(|e| DecodeError::Backend(format!("{e}")))?;

        let n = frames * self.channels as usize;
        Ok(DecodedAudio {
            samples: self.scratch[..n].to_vec(),
            channels: self.channels,
            sample_rate: self.sample_rate,
            pts_ns,
        })
    }

    fn reset(&mut self) {
        // `reset_state` is libopus's `opus_decoder_ctl(OPUS_RESET_STATE)`.
        if let Err(e) = self.inner.reset_state() {
            re_log::warn!("opus decoder reset failed: {e}");
        }
    }

    fn codec(&self) -> AudioCodecKind {
        AudioCodecKind::Opus
    }
}
