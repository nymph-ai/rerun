//! Cache for audio stream players (mirror of [`VideoStreamCache`]).
//!
//! Incremental chunk tracking is deliberately left out of this first cut —
//! the audio visualizer is expected to refresh segments from the store each
//! frame via [`AudioStreamCache::refresh_segments_from_store`]. The cache's
//! role here is to (a) resolve the static config once and construct the
//! [`re_audio::AudioStreamPlayer`], and (b) keep it alive across frames.
//!
//! [`VideoStreamCache`]: crate::VideoStreamCache

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ahash::HashMap;
use parking_lot::Mutex;
use re_audio::{AudioCodecKind, AudioStreamPlayer, SegmentRef, player::PlayerConfig};
use re_byte_size::SizeBytes as _;
use re_chunk::{ComponentIdentifier, EntityPath, TimelineName};
use re_chunk_store::{ChunkStoreEvent, ChunkTrackingMode, RangeQuery};
use re_entity_db::EntityDb;
use re_log_types::{AbsoluteTimeRange, EntityPathHash};
use re_sdk_types::archetypes::AudioStream;
use re_sdk_types::components;

use crate::Cache;

/// Error surfaced while loading static config for an audio stream.
#[derive(Debug, thiserror::Error)]
pub enum AudioStreamProcessingError {
    /// No codec component has been logged yet.
    #[error("missing AudioCodec component on entity")]
    MissingCodec,

    /// No sample rate component has been logged yet.
    #[error("missing AudioSampleRate component on entity")]
    MissingSampleRate,

    /// No channel count component has been logged yet.
    #[error("missing AudioChannelCount component on entity")]
    MissingChannelCount,

    /// Failed to decode a scalar config component.
    #[error("failed to read component {name}: {err}")]
    BadComponent {
        /// Component name.
        name: &'static str,
        /// Error from the chunk layer.
        err: Box<re_chunk::ChunkError>,
    },

    /// Player construction failed.
    #[error("{0}")]
    Player(Box<re_audio::AudioError>),
}

const _: () = assert!(
    std::mem::size_of::<AudioStreamProcessingError>() <= 48,
    "Error type is too large. Try to reduce its size by boxing some of its variants.",
);

/// An audio stream player ready to be driven by the viewer.
pub struct PlayableAudioStream {
    /// The decode/transport runtime.
    pub player: AudioStreamPlayer,

    /// Codec used by this stream. Duplicated here so UI code doesn't need to
    /// reach into `player` for something as small as a badge.
    pub codec: AudioCodecKind,

    /// Source sample rate of the encoded stream, in Hz.
    pub source_rate: u32,

    /// Channel count.
    pub channels: u16,
}

impl re_byte_size::SizeBytes for PlayableAudioStream {
    fn heap_size_bytes(&self) -> u64 {
        // `re_audio` does not yet plumb `SizeBytes` through the player; report
        // 0 here rather than guess.
        0
    }
}

/// A handle to the player shared with the visualizer.
pub type SharablePlayableAudioStream = Arc<Mutex<PlayableAudioStream>>;

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
struct AudioStreamKey {
    entity_path: EntityPathHash,
    timeline: TimelineName,
    chunk_component: ComponentIdentifier,
}

impl re_byte_size::SizeBytes for AudioStreamKey {
    fn heap_size_bytes(&self) -> u64 {
        let Self {
            entity_path,
            timeline,
            chunk_component,
        } = self;
        entity_path.heap_size_bytes()
            + timeline.heap_size_bytes()
            + chunk_component.heap_size_bytes()
    }
}

struct Entry {
    used_this_frame: AtomicBool,
    stream: SharablePlayableAudioStream,
}

impl re_byte_size::SizeBytes for Entry {
    fn heap_size_bytes(&self) -> u64 {
        let Self {
            used_this_frame: _,
            stream,
        } = self;
        stream.lock().heap_size_bytes()
    }
}

/// Caches [`AudioStreamPlayer`] instances per (entity, timeline).
#[derive(Default)]
pub struct AudioStreamCache {
    entries: HashMap<AudioStreamKey, Entry>,
}

impl AudioStreamCache {
    /// Resolve (or cache) the player for an [`rerun.archetypes.AudioStream`] entity.
    ///
    /// `device_rate` and `ring_frames` are properties of the output sink, not
    /// the stream, and are therefore passed by the caller.
    pub fn audio_entry(
        &mut self,
        store: &EntityDb,
        entity_path: &EntityPath,
        timeline: TimelineName,
        device_rate: u32,
        ring_frames: usize,
    ) -> Result<SharablePlayableAudioStream, AudioStreamProcessingError> {
        re_tracing::profile_function!();

        let chunk_component = AudioStream::descriptor_chunk().component;
        let key = AudioStreamKey {
            entity_path: entity_path.hash(),
            timeline,
            chunk_component,
        };

        let entry = match self.entries.entry(key) {
            std::collections::hash_map::Entry::Occupied(occupied) => occupied.into_mut(),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                let config =
                    resolve_static_config(store, entity_path, timeline, device_rate, ring_frames)?;
                let player = AudioStreamPlayer::new(config)
                    .map_err(|e| AudioStreamProcessingError::Player(Box::new(e)))?;
                let stream = Arc::new(Mutex::new(PlayableAudioStream {
                    player,
                    codec: config.codec,
                    source_rate: config.source_rate,
                    channels: config.channels,
                }));
                vacant.insert(Entry {
                    used_this_frame: AtomicBool::new(true),
                    stream,
                })
            }
        };

        entry.used_this_frame.store(true, Ordering::Release);
        Ok(entry.stream.clone())
    }

    /// Rebuild the player's segment index from the store for the given time range.
    ///
    /// This is O(#chunks × #rows) per call; for typical audio volume that is
    /// still cheap. Incremental chunk tracking (a la [`VideoStreamCache`]) is
    /// deferred until the shape of the audio view stabilizes.
    ///
    /// [`VideoStreamCache`]: crate::VideoStreamCache
    pub fn refresh_segments_from_store(
        stream: &SharablePlayableAudioStream,
        store: &EntityDb,
        entity_path: &EntityPath,
        timeline: TimelineName,
        range: AbsoluteTimeRange,
    ) {
        re_tracing::profile_function!();

        let chunk_descr = AudioStream::descriptor_chunk();
        let duration_descr = AudioStream::descriptor_duration_samples();
        let seekable_descr = AudioStream::descriptor_seekable();
        let discontinuity_descr = AudioStream::descriptor_discontinuity();

        let query = RangeQuery::new(timeline, range);
        let results = store.storage_engine().store().range_relevant_chunks(
            ChunkTrackingMode::Ignore,
            &query,
            entity_path,
            chunk_descr.component,
        );

        let mut stream_guard = stream.lock();
        let source_rate = stream_guard.source_rate;
        if source_rate == 0 {
            return;
        }

        let PlayableAudioStream { player, .. } = &mut *stream_guard;
        player.clear_segments();

        for chunk in &results.chunks {
            let chunk_iter = chunk.iter_component::<components::AudioChunk>(chunk_descr.component);
            let duration_iter =
                chunk.iter_component::<components::AudioDurationSamples>(duration_descr.component);
            let seekable_iter =
                chunk.iter_component::<components::AudioSeekable>(seekable_descr.component);
            let discontinuity_iter = chunk
                .iter_component::<components::AudioDiscontinuity>(discontinuity_descr.component);
            let index_iter = chunk.iter_component_indices(timeline, chunk_descr.component);

            for (
                ((((time, _row_id), chunk_item), duration_item), seekable_item),
                discontinuity_item,
            ) in index_iter
                .zip(chunk_iter)
                .zip(duration_iter)
                .zip(seekable_iter)
                .zip(discontinuity_iter)
            {
                let Some(audio_chunk) = chunk_item.as_slice().first() else {
                    continue;
                };
                let bytes = audio_chunk.0.0.to_vec();
                if bytes.is_empty() {
                    continue;
                }

                let pts_ns = time.as_i64();
                let duration_ns = duration_item
                    .as_slice()
                    .first()
                    .map_or(0, |d| samples_to_ns(d.0.0, source_rate));
                let seekable = seekable_item.as_slice().first().is_none_or(|s| s.0.0);
                let discontinuity = discontinuity_item.as_slice().first().is_some_and(|d| d.0.0);

                player.push_segment(SegmentRef {
                    pts_ns,
                    duration_ns,
                    chunk: bytes,
                    seekable,
                    discontinuity,
                });
            }
        }
    }
}

impl Cache for AudioStreamCache {
    fn name(&self) -> &'static str {
        "AudioStreamCache"
    }

    fn begin_frame(&mut self) {
        re_tracing::profile_function!();

        self.entries
            .retain(|_, entry| entry.used_this_frame.load(Ordering::Acquire));

        #[expect(clippy::iter_over_hash_type)]
        for entry in self.entries.values() {
            entry.used_this_frame.store(false, Ordering::Release);
        }
    }

    fn purge_memory(&mut self) {
        self.entries.clear();
    }

    fn on_store_events(&mut self, _events: &[&ChunkStoreEvent], _entity_db: &EntityDb) {
        // Segments are re-read from the store each frame, so there's no stale
        // state to evict on store events.
    }
}

impl re_byte_size::MemUsageTreeCapture for AudioStreamCache {
    fn capture_mem_usage_tree(&self) -> re_byte_size::MemUsageTree {
        let mut node = re_byte_size::MemUsageNode::new();
        node.add(
            "entries",
            re_byte_size::MemUsageTree::Bytes(self.entries.heap_size_bytes()),
        );
        node.with_total_size_bytes(self.entries.total_size_bytes())
    }
}

fn resolve_static_config(
    store: &EntityDb,
    entity_path: &EntityPath,
    timeline: TimelineName,
    device_rate: u32,
    ring_frames: usize,
) -> Result<PlayerConfig, AudioStreamProcessingError> {
    let codec_descr = AudioStream::descriptor_codec();
    let rate_descr = AudioStream::descriptor_sample_rate();
    let channels_descr = AudioStream::descriptor_channel_count();

    let query = re_chunk::LatestAtQuery::new(timeline, re_chunk::TimeInt::MAX);
    let latest = store.storage_engine().cache().latest_at(
        &query,
        entity_path,
        [
            codec_descr.component,
            rate_descr.component,
            channels_descr.component,
        ],
    );

    let codec_chunk = latest
        .get_required(codec_descr.component)
        .map_err(|_err| AudioStreamProcessingError::MissingCodec)?;
    let codec_component = codec_chunk
        .component_mono::<components::AudioCodec>(codec_descr.component)
        .ok_or(AudioStreamProcessingError::MissingCodec)?
        .map_err(|err| AudioStreamProcessingError::BadComponent {
            name: "AudioCodec",
            err: Box::new(err),
        })?;

    let codec = match codec_component {
        components::AudioCodec::Opus => AudioCodecKind::Opus,
        components::AudioCodec::Flac => AudioCodecKind::Flac,
    };

    let rate_chunk = latest
        .get_required(rate_descr.component)
        .map_err(|_err| AudioStreamProcessingError::MissingSampleRate)?;
    let sample_rate = rate_chunk
        .component_mono::<components::AudioSampleRate>(rate_descr.component)
        .ok_or(AudioStreamProcessingError::MissingSampleRate)?
        .map_err(|err| AudioStreamProcessingError::BadComponent {
            name: "AudioSampleRate",
            err: Box::new(err),
        })?;

    let ch_chunk = latest
        .get_required(channels_descr.component)
        .map_err(|_err| AudioStreamProcessingError::MissingChannelCount)?;
    let channel_count = ch_chunk
        .component_mono::<components::AudioChannelCount>(channels_descr.component)
        .ok_or(AudioStreamProcessingError::MissingChannelCount)?
        .map_err(|err| AudioStreamProcessingError::BadComponent {
            name: "AudioChannelCount",
            err: Box::new(err),
        })?;

    Ok(PlayerConfig {
        codec,
        source_rate: sample_rate.0.0,
        channels: channel_count.0.0,
        device_rate,
        ring_frames,
    })
}

fn samples_to_ns(samples: u64, rate: u32) -> i64 {
    if rate == 0 {
        0
    } else {
        i64::try_from(samples.saturating_mul(1_000_000_000) / u64::from(rate)).unwrap_or(i64::MAX)
    }
}
