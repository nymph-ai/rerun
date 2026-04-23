//! Rerun audio View.
//!
//! A View for `AudioStream` and `AssetAudio` archetypes. The view is
//! a thin UI layer: it reads per-segment metadata from the store, surfaces
//! codec / sample rate / channel configuration, and reports diagnostics
//! (sequence gaps, out-of-order segments, discontinuities).
//!
//! Decoding and playback live in [`re_audio`]; the cache wiring into
//! [`re_viewer_context::AudioStreamCache`] is added alongside the native
//! audio output sink.

mod playback_state;
mod view_class;
mod visualizer_system;

pub use playback_state::{AudioViewState, ViewTransport};
pub use view_class::AudioView;
