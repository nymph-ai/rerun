//! Cache for complete audio assets (mirror of [`VideoAssetCache`]).
//!
//! [`VideoAssetCache`]: crate::VideoAssetCache

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ahash::HashMap;
use re_byte_size::SizeBytes as _;
use re_chunk::RowId;
use re_chunk_store::ChunkStoreEvent;
use re_entity_db::EntityDb;
use re_log_types::hash::Hash64;
use re_sdk_types::ComponentIdentifier;
use re_sdk_types::components::MediaType;

use crate::Cache;
use crate::cache::filter_blob_removed_events;
use crate::image_info::StoredBlobCacheKey;

/// Error produced while parsing an [`rerun.archetypes.AssetAudio`] blob.
#[derive(Debug, thiserror::Error, Clone)]
pub enum AudioAssetLoadError {
    /// The media type is not recognized, and `MediaType::guess_from_data` failed
    /// to sniff it from the blob.
    #[error("unrecognized audio media type")]
    UnrecognizedMediaType,
}

/// The "loaded" form of an [`rerun.archetypes.AssetAudio`] — the raw bytes plus
/// resolved media type. Decoding is deferred to the playback runtime.
#[derive(Debug, Clone)]
pub struct PlayableAudioAsset {
    /// IANA media type (e.g. `audio/ogg`).
    pub media_type: MediaType,
    /// Raw encoded bytes.
    pub blob: Arc<re_sdk_types::datatypes::Blob>,
}

impl re_byte_size::SizeBytes for PlayableAudioAsset {
    fn heap_size_bytes(&self) -> u64 {
        let Self { media_type, blob } = self;
        media_type.heap_size_bytes() + blob.heap_size_bytes()
    }
}

impl re_byte_size::SizeBytes for AudioAssetLoadError {
    fn heap_size_bytes(&self) -> u64 {
        0
    }
}

struct Entry {
    used_this_frame: AtomicBool,
    asset: Arc<Result<PlayableAudioAsset, AudioAssetLoadError>>,
    debug_name: String,
}

impl re_byte_size::SizeBytes for Entry {
    fn heap_size_bytes(&self) -> u64 {
        let Self {
            used_this_frame: _,
            asset,
            debug_name,
        } = self;
        debug_name.heap_size_bytes() + asset.heap_size_bytes()
    }
}

/// Caches audio assets keyed by blob row id + media-type resolution.
#[derive(Default)]
pub struct AudioAssetCache(HashMap<StoredBlobCacheKey, HashMap<Hash64, Entry>>);

impl AudioAssetCache {
    /// Resolve (or cache) the asset for a given blob row id + media type override.
    pub fn entry(
        &mut self,
        debug_name: String,
        blob_row_id: RowId,
        blob_component: ComponentIdentifier,
        audio_buffer: &re_sdk_types::datatypes::Blob,
        media_type: Option<&MediaType>,
    ) -> Arc<Result<PlayableAudioAsset, AudioAssetLoadError>> {
        re_tracing::profile_function!(&debug_name);

        let blob_cache_key = StoredBlobCacheKey::new(blob_row_id, blob_component);

        let Some(media_type) = media_type
            .cloned()
            .or_else(|| MediaType::guess_from_data(audio_buffer))
        else {
            return Arc::new(Err(AudioAssetLoadError::UnrecognizedMediaType));
        };

        let inner_key = Hash64::hash(media_type.as_str());

        let entry = self
            .0
            .entry(blob_cache_key)
            .or_default()
            .entry(inner_key)
            .or_insert_with(|| {
                let asset = Ok(PlayableAudioAsset {
                    media_type: media_type.clone(),
                    blob: Arc::new(audio_buffer.clone()),
                });
                Entry {
                    used_this_frame: AtomicBool::new(true),
                    asset: Arc::new(asset),
                    debug_name,
                }
            });

        entry.used_this_frame.store(true, Ordering::Release);
        entry.asset.clone()
    }
}

impl Cache for AudioAssetCache {
    fn name(&self) -> &'static str {
        "AudioAssetCache"
    }

    fn begin_frame(&mut self) {
        re_tracing::profile_function!();

        self.0.retain(|_row_id, per_key| {
            per_key.retain(|_, v| v.used_this_frame.load(Ordering::Acquire));
            !per_key.is_empty()
        });

        #[expect(clippy::iter_over_hash_type)]
        for per_key in self.0.values() {
            for v in per_key.values() {
                v.used_this_frame.store(false, Ordering::Release);
            }
        }
    }

    fn purge_memory(&mut self) {
        // Assets are already aggressively retained only while in use.
    }

    fn on_store_events(&mut self, events: &[&ChunkStoreEvent], _entity_db: &EntityDb) {
        re_tracing::profile_function!();

        let cache_key_removed = filter_blob_removed_events(events);
        self.0
            .retain(|cache_key, _per_key| !cache_key_removed.contains(cache_key));
    }
}

impl re_byte_size::MemUsageTreeCapture for AudioAssetCache {
    fn capture_mem_usage_tree(&self) -> re_byte_size::MemUsageTree {
        let mut node = re_byte_size::MemUsageNode::new();

        let mut items: Vec<_> = self
            .0
            .values()
            .flat_map(|per_key| per_key.values())
            .map(|entry| {
                let size = entry.heap_size_bytes();
                (entry.debug_name.as_str(), size)
            })
            .collect();
        items.sort_by(|a, b| a.0.cmp(b.0));

        for (debug_name, size) in items {
            node.add(debug_name, re_byte_size::MemUsageTree::Bytes(size));
        }

        node.with_total_size_bytes(self.0.total_size_bytes())
    }
}
