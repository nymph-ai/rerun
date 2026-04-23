//! Output-sink abstractions consumed by [`crate::AudioStreamPlayer`] users.
//!
//! Sinks accept interleaved `f32` samples and forward them to whatever
//! platform-specific mixer is available:
//!
//! * `cpal` on native (Linux / macOS / Windows).
//! * `WebAudio` in the browser (wasm32 targets).
//!
//! The sink is deliberately decoupled from the player: the player produces
//! PCM at the device sample rate, and the sink consumes it. This means tests
//! can feed a mock sink without needing an audio device.

#[cfg(all(not(target_arch = "wasm32"), feature = "cpal"))]
pub mod cpal;

#[cfg(all(target_arch = "wasm32", feature = "web_audio"))]
pub mod web_audio;

/// A destination for interleaved-float PCM samples.
///
/// Sinks are **not required** to be `Send`/`Sync` — `cpal::Stream` is
/// intentionally pinned to its creator thread on some platforms. Callers
/// that need to share a sink between threads should wrap it in an
/// `Arc<Mutex<_>>` (or similar) externally.
pub trait AudioSink {
    /// The sample rate the sink is running at, in Hz.
    fn sample_rate(&self) -> u32;

    /// The number of interleaved channels the sink expects.
    fn channels(&self) -> u16;

    /// Copy as many samples as fit into the sink's internal buffer.
    /// Returns the number of samples actually enqueued.
    fn push(&self, interleaved: &[f32]) -> usize;

    /// Drop any samples currently in flight. Called on seek/pause to avoid
    /// playing stale audio.
    fn flush(&self);
}
