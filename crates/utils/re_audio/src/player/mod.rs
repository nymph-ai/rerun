//! Stream player — the media playback runtime.
//!
//! Responsibilities (see the RFC, §7):
//!
//! * resolve viewer-timeline time → media time
//! * schedule decoder calls ahead of the output device
//! * maintain a PCM ring buffer
//! * handle seek, pause, loop, and discontinuity resets
//! * keep the media clock aligned with the timeline within a small bias
//!
//! The player is deliberately codec-agnostic: it takes any `AudioDecoder`.

mod media_clock;
mod sample_ring;
mod segment_index;

pub use media_clock::MediaClock;
pub use sample_ring::SampleRing;
pub use segment_index::{SegmentIndex, SegmentRef};

use crate::{
    AudioError, PtsNs,
    codec::AudioCodecKind,
    decode::{AudioDecoder, DecoderConfig, make_decoder},
    resampler::InterleavedResampler,
};

/// Transport state handed to the player by the viewer each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    /// Normal playback — PCM is generated to meet the playhead.
    Playing,
    /// Paused — ring fades out, no new decoding.
    Paused,
    /// User is scrubbing — audio is muted, no decoding.
    Scrubbing,
}

/// Parameters that stay fixed for the lifetime of the player.
#[derive(Debug, Clone, Copy)]
pub struct PlayerConfig {
    /// Codec of the stream. Picks the decoder backend.
    pub codec: AudioCodecKind,
    /// Sample rate of the encoded stream.
    pub source_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Output device sample rate.
    pub device_rate: u32,
    /// Ring-buffer capacity, in output-rate frames.
    pub ring_frames: usize,
}

/// Audio stream player: runs between the decoder and the output sink.
pub struct AudioStreamPlayer {
    config: PlayerConfig,
    decoder: Box<dyn AudioDecoder>,
    resampler: InterleavedResampler,
    ring: SampleRing,
    media_clock: MediaClock,
    segments: SegmentIndex,
    last_state: TransportState,
    last_playhead_ns: PtsNs,
}

impl AudioStreamPlayer {
    /// Construct a new stream player.
    pub fn new(config: PlayerConfig) -> Result<Self, AudioError> {
        let decoder = make_decoder(DecoderConfig {
            codec: config.codec,
            sample_rate: config.source_rate,
            channels: config.channels,
        })?;

        let resampler =
            InterleavedResampler::new(config.source_rate, config.device_rate, config.channels)
                .map_err(|e| AudioError::Decode(crate::DecodeError::Backend(format!("{e}"))))?;

        Ok(Self {
            config,
            decoder,
            resampler,
            ring: SampleRing::new(config.ring_frames, config.channels as usize),
            media_clock: MediaClock::new(config.source_rate),
            segments: SegmentIndex::new(),
            last_state: TransportState::Paused,
            last_playhead_ns: i64::MIN,
        })
    }

    /// Register a segment. Segments may arrive out-of-order.
    pub fn push_segment(&mut self, seg: SegmentRef) {
        self.segments.insert(seg);
    }

    /// Drop all known segments (e.g. when the entity is replaced).
    pub fn clear_segments(&mut self) {
        self.segments.clear();
    }

    /// Called once per viewer frame with the current transport state + playhead.
    pub fn tick(&mut self, playhead_ns: PtsNs, state: TransportState) -> &mut SampleRing {
        let seek_requested = self.needs_seek(playhead_ns, state);

        match state {
            TransportState::Paused | TransportState::Scrubbing => {
                self.ring.mute_fade();
                self.last_state = state;
                self.last_playhead_ns = playhead_ns;
                return &mut self.ring;
            }
            TransportState::Playing => {}
        }

        if seek_requested && let Some(target) = self.segments.nearest_prior_seekable(playhead_ns) {
            self.decoder.reset();
            self.resampler = InterleavedResampler::new(
                self.config.source_rate,
                self.config.device_rate,
                self.config.channels,
            )
            .expect("resampler rebuild after seek");
            self.media_clock.realign(target.pts_ns, playhead_ns);
            self.ring.flush();
        }

        // Decode until the ring is sufficiently full for the next sink draw.
        while self.ring.available_write() >= DECODE_WATERMARK {
            let next_pts = self.media_clock.pts_ns();
            let Some(seg) = self.segments.next_at_or_after(next_pts) else {
                break;
            };
            if seg.discontinuity {
                self.decoder.reset();
                self.ring.flush();
            }

            match self.decoder.decode(&seg.chunk, seg.pts_ns) {
                Ok(pcm) => {
                    let resampled = self.resampler.process(&pcm.samples);
                    let frames_pushed = if self.config.channels > 0 {
                        resampled.len() / self.config.channels as usize
                    } else {
                        0
                    };
                    self.ring.write(&resampled);
                    self.media_clock.advance_frames(frames_pushed as u64);
                }
                Err(e) => {
                    re_log::warn!("decode failed at {} ns: {e}", seg.pts_ns);
                    self.decoder.reset();
                    self.ring.flush();
                    break;
                }
            }
        }

        self.last_state = state;
        self.last_playhead_ns = playhead_ns;
        &mut self.ring
    }

    fn needs_seek(&self, playhead_ns: PtsNs, state: TransportState) -> bool {
        // Transition into playback always seeks, to guarantee alignment.
        if self.last_state != state && state == TransportState::Playing {
            return true;
        }
        let media_ns = self.media_clock.pts_ns();
        let drift_ns = (playhead_ns - media_ns).abs();
        drift_ns > SEEK_THRESHOLD_NS
    }

    /// Codec + rate + channel summary used by UI badges.
    pub fn summary(&self) -> (AudioCodecKind, u32, u16) {
        (
            self.config.codec,
            self.config.source_rate,
            self.config.channels,
        )
    }
}

/// Keep the ring at least this full (in frames) before yielding.
const DECODE_WATERMARK: usize = 4_096;

/// Hard-seek threshold: drift above this triggers a decoder reset.
const SEEK_THRESHOLD_NS: i64 = 100_000_000; // 100 ms
