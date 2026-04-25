//! A generic [`ChunkStore`] backed by a [`ChunkProvider`].
//!
//! Mirrors [`crate::LazyRrdStore`] but is generic over the backing source.
//! Constructed from the provider's [`RrdManifest`]; physical chunks load on
//! demand via [`Self::load_chunks`] / [`Self::with_autoload`].

use std::sync::Arc;

use ahash::{HashMap, HashMapExt as _};
use nohash_hasher::{IntMap, IntSet};
use parking_lot::RwLock;

use re_chunk::{Chunk, ChunkId};
use re_log_encoding::{RawRrdManifest, RrdManifest};
use re_log_types::{AbsoluteTimeRange, EntityPath, StoreId, Timeline, TimelineName};

use crate::{
    ChunkDeletionReason, ChunkProvider, ChunkStore, ChunkStoreConfig, ChunkStoreHandle,
    ChunkStoreResult, ChunkTrackingMode, EntityTree, ExtractPropertiesError, LatestAtQuery,
    QueryResults, RangeQuery, StoreSchema,
};

/// Per-eviction-cycle accounting returned by [`LazyChunkStore::evict_outside_window`].
///
/// Operators (and tests) read this to confirm the working-set bound is being
/// honored: `evicted` is the count of physical chunks that were dropped on
/// this cycle; `retained` is the post-cycle physical chunk count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EvictionStats {
    pub evicted: usize,
    pub retained: usize,
}

/// A [`ChunkStore`] whose virtual index is populated from a [`ChunkProvider`]'s
/// manifest, and whose physical chunks are loaded on demand via the provider.
///
/// Generalizes [`crate::LazyRrdStore`] over arbitrary backings (RRD file,
/// Lance + S3, in-memory test sources, …). The store itself only knows how to
/// register virtual chunks and ask the provider to materialize them; any
/// backing-specific state (file handles, network clients, codecs) lives inside
/// the provider implementation.
pub struct LazyChunkStore<P: ChunkProvider> {
    store: ChunkStoreHandle,
    provider: P,

    /// Stable across the store's lifetime — every manifest the provider
    /// surfaces shares this id. Cached so [`Self::store_id`] can return a
    /// borrow without dipping into a `RwLock` guard.
    store_id: StoreId,

    /// Locally cached manifest snapshot, kept in sync with the provider via
    /// [`Self::extend_with_manifest`]. Owning the snapshot here lets accessors
    /// hand out cheap `Arc` clones whose contents won't change underfoot,
    /// even when the provider hot-swaps in a longer manifest.
    manifest_slot: RwLock<Arc<RrdManifest>>,
    raw_manifest_slot: RwLock<Arc<RawRrdManifest>>,

    /// Precomputed map from `ChunkId` to manifest row index. Wrapped in a
    /// lock so [`Self::extend_with_manifest`] can swap in a new index after
    /// a live-edge poll.
    chunk_id_to_index: RwLock<HashMap<ChunkId, usize>>,

    /// Precomputed per-chunk timeline ranges. Wrapped for the same reason as
    /// `chunk_id_to_index`.
    timeline_ranges: RwLock<HashMap<ChunkId, IntMap<Timeline, AbsoluteTimeRange>>>,

    /// Latest viewer-visible time range observed per timeline.
    ///
    /// Populated by [`Self::observe_query_cursor`] (typically called from the
    /// gRPC handler when a `query_dataset` request arrives) and consumed by
    /// [`Self::evict_outside_window`] on the eviction janitor's schedule. The
    /// map only ever holds *latest* observations — older entries are
    /// overwritten in place — because the eviction model is "keep what the
    /// viewer is currently looking at", not "keep the union of every range
    /// ever observed".
    cursor: RwLock<HashMap<TimelineName, AbsoluteTimeRange>>,
}

impl<P: ChunkProvider> LazyChunkStore<P> {
    /// Build a new lazy chunk store from a provider.
    ///
    /// Populates the virtual index from `provider.manifest()`. No chunks are
    /// materialized — call [`Self::load_chunks`] to fetch specific chunks, or
    /// rely on [`Self::with_autoload`] to fetch on missing-data signals.
    pub fn new(provider: P) -> Self {
        let manifest = provider.manifest();
        let raw_manifest = provider.raw_manifest();

        // `ALL_DISABLED` matches `LazyRrdStore`: the chunk store is acting as a
        // cache for the underlying provider, and any compaction or GC would
        // invalidate the manifest's chunk-id assumptions.
        let mut store =
            ChunkStore::new(manifest.store_id().clone(), ChunkStoreConfig::ALL_DISABLED);

        #[expect(clippy::let_underscore_must_use)]
        let _ = store.insert_rrd_manifest(Arc::clone(&manifest));

        let chunk_id_to_index: HashMap<ChunkId, usize> = manifest
            .col_chunk_ids()
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i))
            .collect();

        let timeline_ranges = Self::build_timeline_ranges(&manifest);

        let store_id = manifest.store_id().clone();
        Self {
            store: ChunkStoreHandle::new(store),
            provider,
            store_id,
            manifest_slot: RwLock::new(manifest),
            raw_manifest_slot: RwLock::new(raw_manifest),
            chunk_id_to_index: RwLock::new(chunk_id_to_index),
            timeline_ranges: RwLock::new(timeline_ranges),
            cursor: RwLock::new(HashMap::new()),
        }
    }

    fn build_timeline_ranges(
        manifest: &RrdManifest,
    ) -> HashMap<ChunkId, IntMap<Timeline, AbsoluteTimeRange>> {
        let mut result: HashMap<ChunkId, IntMap<Timeline, AbsoluteTimeRange>> = HashMap::new();
        for per_entity in manifest.temporal_map().values() {
            for (timeline, per_component) in per_entity {
                for per_chunk in per_component.values() {
                    for (&chunk_id, entry) in per_chunk {
                        let e = result.entry(chunk_id).or_default();
                        e.entry(*timeline)
                            .and_modify(|existing| {
                                *existing = existing.union(entry.time_range);
                            })
                            .or_insert(entry.time_range);
                    }
                }
            }
        }
        result
    }

    /// The underlying provider.
    #[inline]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Load specific chunks via the provider.
    ///
    /// Already-loaded chunks are filtered out before delegating; insertion
    /// happens after the provider returns, with no store lock held during I/O.
    pub fn load_chunks(&self, chunk_ids: &[ChunkId]) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
        let to_load: Vec<ChunkId> = {
            let guard = self.store.read();
            chunk_ids
                .iter()
                .filter(|id| guard.physical_chunk(id).is_none())
                .copied()
                .collect()
        };

        if to_load.is_empty() {
            return Ok(Vec::new());
        }

        let loaded = self.provider.load_chunks(&to_load)?;

        let mut store = self.store.write();
        for chunk in &loaded {
            store.insert_chunk(chunk)?;
        }

        Ok(loaded)
    }

    /// Materialize every chunk listed in the manifest.
    pub fn load_all_chunks(&self) -> ChunkStoreResult<()> {
        let manifest = self.manifest();
        self.load_chunks(manifest.col_chunk_ids())?;
        Ok(())
    }

    #[inline]
    pub fn schema(&self) -> StoreSchema {
        self.store.read().schema().clone()
    }

    pub fn entity_tree(&self) -> EntityTree {
        self.store.read().entity_tree().clone()
    }

    /// Total chunk count in the manifest (physical + virtual).
    pub fn num_chunks(&self) -> usize {
        self.manifest_slot.read().num_chunks()
    }

    pub fn num_physical_chunks(&self) -> usize {
        self.store.read().num_physical_chunks()
    }

    pub fn has_physical_chunk(&self, chunk_id: &ChunkId) -> bool {
        self.store.read().physical_chunk(chunk_id).is_some()
    }

    /// Load all chunks, then return a compacted copy of the store.
    pub fn compacted(&self, options: &crate::CompactionOptions) -> ChunkStoreResult<ChunkStore> {
        self.load_all_chunks()?;
        self.store.read().compacted(options)
    }

    pub fn collect_physical_chunks(&self) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
        self.load_all_chunks()?;
        Ok(self.store.read().iter_physical_chunks().cloned().collect())
    }

    /// Current manifest snapshot. The returned `Arc` is decoupled from any
    /// future calls to [`Self::extend_with_manifest`].
    #[inline]
    pub fn manifest(&self) -> Arc<RrdManifest> {
        Arc::clone(&self.manifest_slot.read())
    }

    /// Current raw-manifest snapshot. See [`Self::manifest`].
    #[inline]
    pub fn raw_manifest(&self) -> Arc<RawRrdManifest> {
        Arc::clone(&self.raw_manifest_slot.read())
    }

    pub fn chunk_row_index(&self, chunk_id: &ChunkId) -> Option<usize> {
        self.chunk_id_to_index.read().get(chunk_id).copied()
    }

    /// Per-chunk timeline range, cloned out of the live snapshot. Returns
    /// `None` for chunks that are static-only or not present in the current
    /// manifest.
    pub fn timeline_range(
        &self,
        chunk_id: &ChunkId,
    ) -> Option<IntMap<Timeline, AbsoluteTimeRange>> {
        self.timeline_ranges.read().get(chunk_id).cloned()
    }

    pub fn store_id(&self) -> &StoreId {
        // Stable for the store's lifetime — every manifest the provider
        // produces shares this id, so we keep an owned copy and hand out
        // borrows without locking the swappable manifest slot.
        &self.store_id
    }

    /// Replace the cached manifest with a newer snapshot from the provider.
    ///
    /// The new manifest is expected to be a *superset* of the current one
    /// (live-edge polling appends rows; it never rewrites history). The
    /// underlying [`ChunkStore`] is updated via the idempotent
    /// `insert_rrd_manifest` path, which leaves already-loaded physical
    /// chunks intact.
    pub fn extend_with_manifest(
        &self,
        manifest: Arc<RrdManifest>,
        raw_manifest: Arc<RawRrdManifest>,
    ) {
        {
            let mut store = self.store.write();
            #[expect(clippy::let_underscore_must_use)]
            let _ = store.insert_rrd_manifest(Arc::clone(&manifest));
        }

        let new_index: HashMap<ChunkId, usize> = manifest
            .col_chunk_ids()
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i))
            .collect();
        let new_ranges = Self::build_timeline_ranges(&manifest);

        *self.chunk_id_to_index.write() = new_index;
        *self.timeline_ranges.write() = new_ranges;
        *self.manifest_slot.write() = manifest;
        *self.raw_manifest_slot.write() = raw_manifest;
    }

    pub fn all_entities(&self) -> IntSet<EntityPath> {
        self.store.read().all_entities()
    }

    pub fn physical_chunk(&self, id: &ChunkId) -> Option<Arc<Chunk>> {
        self.store.read().physical_chunk(id).cloned()
    }

    pub fn extract_properties(&self) -> Result<arrow::array::RecordBatch, ExtractPropertiesError> {
        self.with_autoload(|store| store.extract_properties())
    }

    /// Run an operation against the inner [`ChunkStore`], auto-loading any
    /// chunks the operation reports as missing and retrying. Mirrors
    /// [`crate::LazyRrdStore::with_autoload`] verbatim.
    fn with_autoload<T, F>(&self, mut op: F) -> Result<T, ExtractPropertiesError>
    where
        F: FnMut(&ChunkStore) -> Result<T, ExtractPropertiesError>,
    {
        const MAX_AUTOLOAD_ATTEMPTS: usize = 1024;
        for _ in 0..MAX_AUTOLOAD_ATTEMPTS {
            let result = op(&self.store.read());
            match result {
                Err(ExtractPropertiesError::MissingData(missing_ids)) => {
                    self.load_chunks(&missing_ids)
                        .map_err(|err| ExtractPropertiesError::Internal(err.to_string()))?;
                }
                other => return other,
            }
        }
        Err(ExtractPropertiesError::Internal(format!(
            "autoload did not converge after {MAX_AUTOLOAD_ATTEMPTS} attempts"
        )))
    }

    pub fn latest_at_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &LatestAtQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults {
        self.store
            .read()
            .latest_at_relevant_chunks_for_all_components(
                report_mode,
                query,
                entity_path,
                include_static,
            )
    }

    pub fn range_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &RangeQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults {
        self.store.read().range_relevant_chunks_for_all_components(
            report_mode,
            query,
            entity_path,
            include_static,
        )
    }

    /// Drop physical chunks within `drop_range` on the given timeline. Chunks
    /// remain virtual and reloadable via the provider — this is the eviction
    /// primitive used by cursor-driven working-set management.
    pub fn drop_time_range_shallow(
        &self,
        timeline: &TimelineName,
        drop_range: AbsoluteTimeRange,
    ) -> Vec<crate::ChunkStoreEvent> {
        self.store.write().drop_time_range_shallow(
            timeline,
            drop_range,
            ChunkDeletionReason::Evicted,
        )
    }

    /// Record the viewer's currently-visible time range on `timeline`.
    ///
    /// Called from the server's `query_dataset` handler so the store sees
    /// every cursor / range query the viewer makes — no extra RPC needed.
    /// Repeated calls overwrite the previous observation; callers that want
    /// to merge multiple cursors should compute the union themselves.
    pub fn observe_query_cursor(&self, timeline: TimelineName, range: AbsoluteTimeRange) {
        self.cursor.write().insert(timeline, range);
    }

    /// Most recent observed cursor / range on `timeline`, if any.
    pub fn cursor(&self, timeline: &TimelineName) -> Option<AbsoluteTimeRange> {
        self.cursor.read().get(timeline).copied()
    }

    /// Number of timelines for which a cursor has been observed.
    pub fn num_cursor_observations(&self) -> usize {
        self.cursor.read().len()
    }

    /// Drop physical chunks whose entire timeline range falls *outside*
    /// `keep_window` on `timeline`. Chunks remain virtual and reloadable —
    /// readers that scroll back into the dropped range trigger a fresh
    /// `provider.load_chunks(...)` via the autoload path.
    ///
    /// Unlike [`Self::drop_time_range_shallow`], this primitive never splits
    /// a chunk: only chunks fully contained in the to-evict region are
    /// removed. That matters for provider-backed stores where a split would
    /// strand an in-memory residue with no manifest backing.
    ///
    /// Static chunks and chunks not present on `timeline` are always
    /// retained.
    pub fn evict_outside_window(
        &self,
        timeline: &TimelineName,
        keep_window: AbsoluteTimeRange,
    ) -> EvictionStats {
        let to_evict: Vec<Arc<Chunk>> = {
            let store = self.store.read();
            let timeline_ranges = self.timeline_ranges.read();
            store
                .iter_physical_chunks()
                .filter(|chunk| {
                    let Some(per_timeline) = timeline_ranges.get(&chunk.id()) else {
                        return false;
                    };
                    let Some(range) = per_timeline
                        .iter()
                        .find_map(|(tl, r)| (tl.name() == timeline).then_some(*r))
                    else {
                        return false;
                    };
                    // Fully outside the keep window on either side?
                    range.max() < keep_window.min() || range.min() > keep_window.max()
                })
                .cloned()
                .collect()
        };

        if to_evict.is_empty() {
            return EvictionStats {
                evicted: 0,
                retained: self.num_physical_chunks(),
            };
        }

        let evicted = to_evict.len();
        {
            let mut store = self.store.write();
            store.remove_chunks_shallow(to_evict, None, ChunkDeletionReason::Evicted);
        }
        EvictionStats {
            evicted,
            retained: self.num_physical_chunks(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use re_byte_size::SizeBytes as _;
    use re_chunk::{Chunk, RowId, Span, TimePoint, Timeline};
    use re_log_encoding::{RawRrdManifest, RrdManifest, RrdManifestBuilder};
    use re_log_types::{
        EntityPath, StoreId, StoreKind,
        example_components::{MyPoint, MyPoints},
    };

    use super::*;

    /// In-memory provider — keeps every chunk in a `BTreeMap<ChunkId, Arc<Chunk>>`,
    /// surfaces a manifest built via [`RrdManifestBuilder`], and serves chunks
    /// directly from the map. Used to validate the trait + autoload plumbing
    /// without dragging in a real backing store.
    struct InMemoryProvider {
        raw: Arc<RawRrdManifest>,
        manifest: Arc<RrdManifest>,
        chunks: HashMap<ChunkId, Arc<Chunk>>,
        load_count: Mutex<usize>,
    }

    impl InMemoryProvider {
        fn new(store_id: StoreId, chunks: Vec<Arc<Chunk>>) -> Self {
            let mut builder = RrdManifestBuilder::default();
            // Synthetic offsets are fine: load_chunks ignores byte offsets and
            // just looks up by ChunkId.
            let mut offset = 0u64;
            for chunk in &chunks {
                let batch = chunk.to_chunk_batch().expect("chunk → batch");
                let size = chunk.heap_size_bytes();
                let span = Span {
                    start: offset,
                    len: size,
                };
                offset += size;
                builder.append(&batch, span, size).expect("manifest append");
            }
            let raw = Arc::new(builder.build(store_id).expect("manifest build"));
            let manifest = Arc::new(RrdManifest::try_new(&raw).expect("manifest validate"));

            let chunks: HashMap<ChunkId, Arc<Chunk>> =
                chunks.into_iter().map(|c| (c.id(), c)).collect();

            Self {
                raw,
                manifest,
                chunks,
                load_count: Mutex::new(0),
            }
        }

        fn load_count(&self) -> usize {
            *self.load_count.lock().unwrap()
        }
    }

    impl ChunkProvider for InMemoryProvider {
        fn manifest(&self) -> Arc<RrdManifest> {
            Arc::clone(&self.manifest)
        }

        fn raw_manifest(&self) -> Arc<RawRrdManifest> {
            Arc::clone(&self.raw)
        }

        fn load_chunks(&self, chunk_ids: &[ChunkId]) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
            *self.load_count.lock().unwrap() += chunk_ids.len();
            Ok(chunk_ids
                .iter()
                .filter_map(|id| self.chunks.get(id).cloned())
                .collect())
        }
    }

    fn make_chunks(num_entities: usize, num_frames: usize) -> (StoreId, Vec<Arc<Chunk>>) {
        let store_id = StoreId::random(StoreKind::Recording, "test");
        let timeline = Timeline::new_sequence("frame");
        let mut chunks = Vec::new();
        for entity_idx in 0..num_entities {
            for frame_idx in 0..num_frames {
                let entity_path = EntityPath::from(format!("/entity_{entity_idx}"));
                let row_id = RowId::new();
                let points = MyPoint::from_iter(frame_idx as u32..frame_idx as u32 + 1);
                let chunk = Chunk::builder(entity_path)
                    .with_sparse_component_batches(
                        row_id,
                        #[expect(clippy::cast_possible_wrap)]
                        TimePoint::default().with(timeline, frame_idx as i64),
                        [(MyPoints::descriptor_points(), Some(&points as _))],
                    )
                    .build()
                    .unwrap();
                chunks.push(Arc::new(chunk));
            }
        }
        (store_id, chunks)
    }

    #[test]
    fn virtual_index_populated_without_loading() {
        let (store_id, chunks) = make_chunks(2, 3);
        let total = chunks.len();
        let provider = InMemoryProvider::new(store_id, chunks);
        let lazy = LazyChunkStore::new(provider);

        assert_eq!(lazy.num_chunks(), total);
        assert_eq!(lazy.num_physical_chunks(), 0);
        assert_eq!(lazy.provider().load_count(), 0);
    }

    #[test]
    fn load_chunks_filters_already_loaded() {
        let (store_id, chunks) = make_chunks(1, 3);
        let provider = InMemoryProvider::new(store_id, chunks);
        let lazy = LazyChunkStore::new(provider);

        let ids: Vec<ChunkId> = lazy.manifest().col_chunk_ids().to_vec();
        let first_round = lazy.load_chunks(&ids).unwrap();
        assert_eq!(first_round.len(), ids.len());
        assert_eq!(lazy.provider().load_count(), ids.len());

        // Second call: already physical, so the provider must not be hit.
        let second_round = lazy.load_chunks(&ids).unwrap();
        assert!(second_round.is_empty());
        assert_eq!(
            lazy.provider().load_count(),
            ids.len(),
            "second round should not re-hit the provider"
        );
    }

    #[test]
    fn load_all_then_iterate_physical_chunks() {
        let (store_id, chunks) = make_chunks(2, 4);
        let total = chunks.len();
        let provider = InMemoryProvider::new(store_id, chunks);
        let lazy = LazyChunkStore::new(provider);

        let collected = lazy.collect_physical_chunks().unwrap();
        assert_eq!(collected.len(), total);
        assert_eq!(lazy.num_physical_chunks(), total);
    }

    #[test]
    fn extend_with_manifest_grows_virtual_index() {
        // Two non-overlapping batches of chunks for the same store_id —
        // the second batch represents what a live-edge poll would surface.
        let (store_id, mut chunks) = make_chunks(1, 3);
        let initial_total = chunks.len();
        let extra = chunks.split_off(2);
        let initial_chunks = chunks;
        let extra_total = extra.len();

        // Build the initial provider with just the first slice; it owns
        // the partial manifest the lazy store will absorb on construction.
        let initial_provider = InMemoryProvider::new(store_id.clone(), initial_chunks.clone());
        let initial_chunk_ids: Vec<ChunkId> = initial_provider.manifest.col_chunk_ids().to_vec();

        // Build a *full* provider over every chunk; we'll borrow its
        // manifests as the "after the poll" snapshot for `extend_with_manifest`.
        let mut full_chunks = initial_chunks;
        full_chunks.extend(extra);
        let full_provider = InMemoryProvider::new(store_id, full_chunks);
        let extended_manifest = Arc::clone(&full_provider.manifest);
        let extended_raw = Arc::clone(&full_provider.raw);

        let lazy = LazyChunkStore::new(initial_provider);
        assert_eq!(lazy.num_chunks(), initial_total - extra_total);
        for &id in &initial_chunk_ids {
            assert!(lazy.chunk_row_index(&id).is_some());
        }

        lazy.extend_with_manifest(extended_manifest, extended_raw);
        assert_eq!(lazy.num_chunks(), initial_total);
        // Every chunk now has a row index, including the freshly absorbed ones.
        for chunk_id in lazy.manifest().col_chunk_ids() {
            assert!(
                lazy.chunk_row_index(chunk_id).is_some(),
                "chunk {chunk_id} missing from extended index"
            );
        }
    }

    #[test]
    fn drop_time_range_shallow_evicts_physical_chunks() {
        let (store_id, chunks) = make_chunks(1, 5);
        let provider = InMemoryProvider::new(store_id, chunks);
        let lazy = LazyChunkStore::new(provider);

        lazy.load_all_chunks().unwrap();
        let loaded_before = lazy.num_physical_chunks();
        assert!(loaded_before > 0);

        // Drop everything by passing a range that spans all valid time
        // values. After eviction, physical count drops back to zero but the
        // virtual count stays the same — chunks remain reloadable.
        let timeline = re_log_types::TimelineName::from("frame");
        let drop_range = AbsoluteTimeRange::EVERYTHING;
        let virtual_before = lazy.num_chunks();
        let _events = lazy.drop_time_range_shallow(&timeline, drop_range);
        assert_eq!(lazy.num_physical_chunks(), 0, "evicted physical chunks");
        assert_eq!(lazy.num_chunks(), virtual_before, "virtual count unchanged");

        // Reload to demonstrate the round-trip.
        lazy.load_all_chunks().unwrap();
        assert_eq!(lazy.num_physical_chunks(), loaded_before);
    }

    #[test]
    fn evict_outside_window_drops_only_fully_out_of_window_chunks() {
        // 1 entity × 5 frames → chunks at sequence times 0..=4.
        let (store_id, chunks) = make_chunks(1, 5);
        let provider = InMemoryProvider::new(store_id, chunks);
        let lazy = LazyChunkStore::new(provider);
        lazy.load_all_chunks().unwrap();

        let timeline = TimelineName::from("frame");
        let initial_physical = lazy.num_physical_chunks();
        assert!(initial_physical >= 5);

        // Keep window = [2, 3]. Chunks at 0, 1 (max < 2) and 4 (min > 3) are
        // fully outside and should be evicted; chunks at 2 and 3 are kept.
        let keep = AbsoluteTimeRange::new(2_i64, 3_i64);
        let stats = lazy.evict_outside_window(&timeline, keep);
        assert!(stats.evicted >= 3, "expected ≥3 evictions, got {stats:?}");
        assert_eq!(stats.retained, lazy.num_physical_chunks());
        // Virtual count stays the same — eviction is shallow.
        let virtual_count = lazy.num_chunks();
        assert!(virtual_count >= 5);

        // Re-evicting with the same window is a no-op (already at steady state).
        let stats = lazy.evict_outside_window(&timeline, keep);
        assert_eq!(stats.evicted, 0);
    }

    #[test]
    fn evict_outside_window_no_op_without_loaded_chunks() {
        let (store_id, chunks) = make_chunks(1, 3);
        let provider = InMemoryProvider::new(store_id, chunks);
        let lazy = LazyChunkStore::new(provider);

        // Nothing is physical yet — eviction should report (0, 0).
        let timeline = TimelineName::from("frame");
        let stats = lazy.evict_outside_window(&timeline, AbsoluteTimeRange::EMPTY);
        assert_eq!(stats.evicted, 0);
        assert_eq!(stats.retained, 0);
    }

    #[test]
    fn observe_query_cursor_records_latest() {
        let (store_id, chunks) = make_chunks(1, 1);
        let provider = InMemoryProvider::new(store_id, chunks);
        let lazy = LazyChunkStore::new(provider);

        let timeline = TimelineName::from("frame");
        assert_eq!(lazy.cursor(&timeline), None);
        assert_eq!(lazy.num_cursor_observations(), 0);

        lazy.observe_query_cursor(timeline, AbsoluteTimeRange::new(10_i64, 20_i64));
        assert_eq!(
            lazy.cursor(&timeline),
            Some(AbsoluteTimeRange::new(10_i64, 20_i64))
        );

        // Newer observation overwrites the old one — eviction always tracks
        // the *current* viewport, not the union of all viewports ever shown.
        lazy.observe_query_cursor(timeline, AbsoluteTimeRange::new(50_i64, 60_i64));
        assert_eq!(
            lazy.cursor(&timeline),
            Some(AbsoluteTimeRange::new(50_i64, 60_i64))
        );
        assert_eq!(lazy.num_cursor_observations(), 1);
    }
}
