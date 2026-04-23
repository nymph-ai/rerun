//! Per-view playback state: owns the output sink and drives the player each frame.
//!
//! One [`AudioViewState`] lives per Audio view instance. It is responsible for:
//!
//! * opening a `cpal` output device on first use (matching the primary stream's
//!   source rate and channel count),
//! * ticking the cached [`AudioStreamPlayer`] against the viewer's timeline
//!   playhead each frame,
//! * draining the resulting PCM into the sink.
//!
//! Mix is intentionally simple for v1: only the first summary with a valid
//! config drives the sink. When multiple audio entities are present in the
//! same view the secondary ones still render their diagnostic panel but do not
//! emit audio.

use re_audio::{TransportState, output::AudioSink as _};
use re_log_types::EntityPath;
use re_viewer_context::{PlayableAudioStream, SharablePlayableAudioStream, ViewState};

#[cfg(not(target_arch = "wasm32"))]
use re_audio::output::cpal::SpawnedCpalSink;
#[cfg(not(target_arch = "wasm32"))]
type PlatformSink = SpawnedCpalSink;

#[cfg(target_arch = "wasm32")]
use re_audio::output::web_audio::WebAudioSink;
#[cfg(target_arch = "wasm32")]
type PlatformSink = WebAudioSink;

#[cfg(not(target_arch = "wasm32"))]
fn open_sink(rate: u32, channels: u16) -> Option<PlatformSink> {
    let ring_frames = rate.max(1) as usize;
    match SpawnedCpalSink::open(Some(rate), Some(channels), ring_frames) {
        Ok(sink) => {
            re_log::debug!(
                "AudioView: opened cpal sink at {} Hz, {} ch",
                sink.sample_rate(),
                sink.channels()
            );
            Some(sink)
        }
        Err(err) => {
            re_log::warn_once!("AudioView: failed to open cpal output: {err}");
            None
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn open_sink(rate: u32, channels: u16) -> Option<PlatformSink> {
    match WebAudioSink::open(rate, channels) {
        Ok(sink) => {
            re_log::debug!(
                "AudioView: opened WebAudio sink at {} Hz, {} ch",
                sink.sample_rate(),
                sink.channels()
            );
            Some(sink)
        }
        Err(err) => {
            re_log::warn_once!("AudioView: failed to open WebAudio output: {err}");
            None
        }
    }
}

/// Transport override set by the view UI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ViewTransport {
    /// Follow the viewer's global play state.
    #[default]
    FollowTimeline,
    /// Muted — player is ticked as Paused regardless of timeline.
    Muted,
}

/// Per-view playback state.
#[derive(Default)]
pub struct AudioViewState {
    /// Default output sink. Opened lazily on first successful tick.
    sink: Option<PlatformSink>,

    /// Entity that currently drives the sink. If the list of summaries changes
    /// and the owner disappears, the sink is flushed and re-acquired by the
    /// first available entity next frame.
    sink_owner: Option<EntityPath>,

    /// Reusable scratch buffer — avoids re-allocating every frame.
    scratch: Vec<f32>,

    /// Transport override from the UI.
    pub transport: ViewTransport,
}

impl ViewState for AudioViewState {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl AudioViewState {
    /// Drive playback for `entity` against `stream`. Returns `true` if this
    /// state is currently sinking audio for that entity (so the UI can mark it
    /// as the active source).
    pub fn drive(
        &mut self,
        entity: &EntityPath,
        stream: &SharablePlayableAudioStream,
        playhead_ns: i64,
        timeline_is_playing: bool,
    ) -> bool {
        let desired_state = match (self.transport, timeline_is_playing) {
            (ViewTransport::Muted, _) | (_, false) => TransportState::Paused,
            (ViewTransport::FollowTimeline, true) => TransportState::Playing,
        };

        let mut guard = stream.lock();
        let PlayableAudioStream {
            player,
            source_rate,
            channels,
            ..
        } = &mut *guard;

        // Claim sink ownership for the first entity we see each frame.
        let owns_sink = match &self.sink_owner {
            Some(owner) if owner == entity => true,
            Some(_) => false,
            None => {
                self.sink_owner = Some(entity.clone());
                true
            }
        };

        if !owns_sink {
            // Just tick so the ring stays alive for the owner entity.
            let _ = player.tick(playhead_ns, desired_state);
            return false;
        }

        if self.sink.is_none() {
            self.sink = open_sink(*source_rate, *channels);
        }

        let Some(sink) = &self.sink else {
            return false;
        };

        let ring = player.tick(playhead_ns, desired_state);

        // Drain as many frames as the ring currently has, up to one tick's
        // worth. We size the scratch at 128 ms of device-rate samples so a
        // single 60 fps frame can always absorb what the player produced.
        let max_samples = sink.sample_rate() as usize * sink.channels() as usize / 8;
        let avail_samples = ring.available_read() * sink.channels() as usize;
        let take = avail_samples.min(max_samples);
        if take == 0 {
            return true;
        }

        if self.scratch.len() < take {
            self.scratch.resize(take, 0.0);
        }
        let buf = &mut self.scratch[..take];
        ring.drain_into(buf);
        sink.push(buf);
        true
    }

    /// Forget the active sink owner — called when the view loses its summaries
    /// (e.g., entity was deleted) so the next frame re-claims ownership.
    pub fn forget_owner_if_missing(&mut self, live_entities: &[EntityPath]) {
        let gone = match &self.sink_owner {
            Some(owner) => !live_entities.iter().any(|e| e == owner),
            None => false,
        };
        if gone {
            self.sink_owner = None;
            if let Some(sink) = &self.sink {
                sink.flush();
            }
        }
    }
}
