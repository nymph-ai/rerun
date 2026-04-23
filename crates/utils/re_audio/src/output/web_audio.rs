//! `WebAudio`-backed output sink (wasm32 only).
//!
//! Browser audio is single-threaded and event-loop driven. We don't have
//! access to a tight pull-model callback like `cpal` gives us; instead we
//! push each incoming PCM buffer into a short-lived [`AudioBufferSourceNode`]
//! and schedule it to start where the last one ended. The browser mixer
//! interleaves those scheduled starts into continuous playback.
//!
//! The sink is single-threaded by construction (wasm has no real threads),
//! but implements `Send + Sync` so it can live inside a `Send + Sync`
//! [`re_viewer_context::ViewState`]. The `unsafe impl`s are sound because
//! there is no way to access the same JS objects from two OS threads.

use std::sync::Arc;

use parking_lot::Mutex;
use wasm_bindgen::JsCast as _;
use web_sys::{
    AudioBuffer, AudioBufferOptions, AudioBufferSourceNode, AudioContext, AudioContextOptions,
    AudioContextState, AudioScheduledSourceNode,
};

use super::AudioSink;

/// Errors surfaced while building a [`WebAudioSink`].
#[derive(Debug, thiserror::Error)]
pub enum WebAudioSinkError {
    /// Creating the `AudioContext` failed (most commonly: WebAudio is not
    /// available in this browser / JS environment).
    #[error("failed to create AudioContext: {0}")]
    CreateContext(String),

    /// Allocating an `AudioBuffer` failed.
    #[error("failed to create AudioBuffer: {0}")]
    CreateBuffer(String),

    /// Creating an `AudioBufferSourceNode` failed.
    #[error("failed to create AudioBufferSourceNode: {0}")]
    CreateSource(String),
}

/// Minimum scheduling lead time, in seconds, added when the ring has
/// underrun. Scheduling precisely at `currentTime` risks being too late and
/// dropping the buffer silently; 20 ms matches an Opus frame.
const MIN_SCHEDULE_LEAD_SEC: f64 = 0.020;

struct Shared {
    /// The last time we scheduled a buffer to end. Monotonically increases
    /// while the stream is running.
    next_start_sec: f64,

    /// Pending source nodes, kept alive so they can be `.stop()`'d on flush.
    /// Drained opportunistically when they finish playing.
    live_sources: Vec<AudioBufferSourceNode>,
}

/// WebAudio output sink backed by an [`AudioContext`] on the main thread.
pub struct WebAudioSink {
    ctx: AudioContext,
    sample_rate: u32,
    channels: u16,
    shared: Arc<Mutex<Shared>>,
}

impl WebAudioSink {
    /// Open a new `AudioContext` at `sample_rate`.
    ///
    /// Browsers may refuse to construct the context or refuse to resume it
    /// until a user gesture has occurred. In either case the sink returns
    /// successfully but silently drops samples until `resume()` succeeds;
    /// callers typically trigger that on the first user interaction.
    pub fn open(sample_rate: u32, channels: u16) -> Result<Self, WebAudioSinkError> {
        let options = AudioContextOptions::new();
        options.set_sample_rate(sample_rate as f32);

        let ctx = AudioContext::new_with_context_options(&options)
            .map_err(|e| WebAudioSinkError::CreateContext(format!("{e:?}")))?;

        // Best-effort resume. If we're still in the pre-gesture "suspended"
        // state this will fail silently — the sink will start mixing as soon
        // as the user interacts with the page.
        let _ = ctx.resume();

        Ok(Self {
            ctx,
            sample_rate,
            channels,
            shared: Arc::new(Mutex::new(Shared {
                next_start_sec: 0.0,
                live_sources: Vec::new(),
            })),
        })
    }

    fn schedule(&self, interleaved: &[f32]) -> Result<(), WebAudioSinkError> {
        if interleaved.is_empty() || self.channels == 0 {
            return Ok(());
        }

        let channels = self.channels as usize;
        let frames = interleaved.len() / channels;
        if frames == 0 {
            return Ok(());
        }

        let buffer_options = AudioBufferOptions::new(frames as u32, self.sample_rate as f32);
        buffer_options.set_number_of_channels(self.channels as u32);
        let buffer = AudioBuffer::new(&buffer_options)
            .map_err(|e| WebAudioSinkError::CreateBuffer(format!("{e:?}")))?;

        // Deinterleave into per-channel planar buffers.
        let mut plane = vec![0.0_f32; frames];
        for ch in 0..channels {
            for i in 0..frames {
                plane[i] = interleaved[i * channels + ch];
            }
            // `copy_to_channel` copies the slice into the AudioBuffer.
            let _ = buffer.copy_to_channel(&mut plane, ch as i32);
        }

        let source = self
            .ctx
            .create_buffer_source()
            .map_err(|e| WebAudioSinkError::CreateSource(format!("{e:?}")))?;
        source.set_buffer(Some(&buffer));
        source
            .connect_with_audio_node(&self.ctx.destination())
            .map_err(|e| WebAudioSinkError::CreateSource(format!("{e:?}")))?;

        let now = self.ctx.current_time();
        let duration = frames as f64 / self.sample_rate as f64;

        let mut shared = self.shared.lock();

        // Clamp scheduling to at least now + lead time; if we fell behind
        // (e.g., the tab was backgrounded), resync to the current clock to
        // avoid accumulating latency.
        let start = shared.next_start_sec.max(now + MIN_SCHEDULE_LEAD_SEC);

        let _ = source.start_with_when(start);
        shared.next_start_sec = start + duration;

        // Track the source so flush() can stop it early. We don't try to
        // prune finished nodes here — the browser releases them internally
        // and `stop()` on an already-finished node is a no-op.
        shared.live_sources.push(source);

        Ok(())
    }
}

impl AudioSink for WebAudioSink {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn push(&self, interleaved: &[f32]) -> usize {
        // WebAudio has no hard ring-size limit; always accept the entire
        // buffer. The browser is free to drop chunks that fall too far behind
        // `currentTime`, which we guard against by syncing `next_start_sec`
        // forward on each push.
        if let Err(err) = self.schedule(interleaved) {
            re_log::warn_once!("WebAudioSink: failed to schedule buffer: {err}");
            return 0;
        }
        interleaved.len()
    }

    fn flush(&self) {
        let mut shared = self.shared.lock();
        for source in shared.live_sources.drain(..) {
            // Go through the base class to avoid the deprecated
            // `AudioBufferSourceNode::stop` shim in web-sys.
            let scheduled: &AudioScheduledSourceNode = source.unchecked_ref();
            let _ = scheduled.stop();
        }
        shared.next_start_sec = 0.0;
    }
}

// SAFETY: wasm32 has no real OS threads — every handle we hold is confined
// to the main browser thread. The `unsafe impl`s exist only so that
// `WebAudioSink` can live inside a `Send + Sync` container.
#[expect(unsafe_code)]
#[expect(clippy::undocumented_unsafe_blocks)]
unsafe impl Send for WebAudioSink {}
#[expect(unsafe_code)]
#[expect(clippy::undocumented_unsafe_blocks)]
unsafe impl Sync for WebAudioSink {}

impl Drop for WebAudioSink {
    fn drop(&mut self) {
        let mut shared = self.shared.lock();
        for source in shared.live_sources.drain(..) {
            let scheduled: &AudioScheduledSourceNode = source.unchecked_ref();
            let _ = scheduled.stop();
        }
        let _ = self.ctx.close();
    }
}

/// Best-effort resume of the underlying `AudioContext`.
///
/// Browsers suspend the context until a user gesture has occurred. Call this
/// from a gesture handler (e.g., the first click on the viewer canvas) to
/// un-suspend playback.
pub fn try_resume(sink: &WebAudioSink) {
    if sink.ctx.state() == AudioContextState::Suspended {
        let _ = sink.ctx.resume();
    }
}
