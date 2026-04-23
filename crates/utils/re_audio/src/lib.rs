//! Rerun audio decoding and playback runtime.
//!
//! This crate is the audio analog of [`re_video`]: it owns the decode path
//! (native libopus + `WebCodecs`), stream-level segment indexing, the
//! playback clock, and the PCM ring buffer that is drained by an output
//! sink such as `cpal` or a `WebAudio` `AudioWorklet`.
//!
//! The crate is deliberately agnostic of Rerun's component/archetype types:
//! callers pass in encoded segments tagged with `pts_ns` and receive PCM
//! frames tagged with the same time base. Wiring to `AudioStream` /
//! `AssetAudio` lives in the viewer.
//!
//! # Coordinate systems
//!
//! * **Timeline time** — nanoseconds on whichever `TimeIndex` the viewer is
//!   driving. The viewer treats this as the source of truth for the playhead.
//! * **Media time** — nanoseconds relative to the start of the stream; equal
//!   to `sample_index * 1e9 / sample_rate`. The stream player converts
//!   between these using a `MediaClock`.
//!
//! [`re_video`]: https://docs.rs/re_video

#![warn(missing_docs)]

pub mod codec;
pub mod decode;
pub mod output;
pub mod player;
pub mod resampler;

mod error;

pub use codec::{AudioCodecKind, ChannelLayout};
pub use decode::{AudioDecoder, DecodedAudio, DecoderConfig, DecoderFactory, DecoderRegistry};
pub use error::{AudioError, DecodeError};
pub use player::{AudioStreamPlayer, SegmentRef, TransportState};

/// Presentation timestamp in nanoseconds.
pub type PtsNs = i64;
