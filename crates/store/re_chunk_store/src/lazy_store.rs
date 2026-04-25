//! Object-safe trait abstracting over [`crate::LazyRrdStore`] and
//! [`crate::LazyChunkStore`].
//!
//! Both back `re_server`'s [`crate::ResolvedStore::Lazy`] arm; this trait is
//! the union of the methods that arm needs. New backings (corpus producer,
//! cloud-hosted indexes, …) implement [`LazyStore`] and slot into existing
//! infrastructure without per-call-site enum churn.

use std::sync::Arc;

use arrow::array::RecordBatch;
use nohash_hasher::{IntMap, IntSet};

use re_chunk::{Chunk, ChunkId};
use re_log_encoding::{RawRrdManifest, RrdManifest};
use re_log_types::{AbsoluteTimeRange, EntityPath, StoreId, Timeline, TimelineName};

use crate::{
    ChunkStoreResult, ChunkTrackingMode, ExtractPropertiesError, LatestAtQuery, QueryResults,
    RangeQuery, StoreSchema,
};

/// Common surface for stores whose virtual index is populated up front and
/// whose physical chunks materialize on demand.
///
/// Object-safe so `re_server` can hold an `Arc<dyn LazyStore>` without
/// committing to a specific backing (RRD file, Lance + S3, …).
pub trait LazyStore: Send + Sync + 'static {
    fn store_id(&self) -> &StoreId;
    fn schema(&self) -> StoreSchema;
    fn all_entities(&self) -> IntSet<EntityPath>;
    fn physical_chunk(&self, id: &ChunkId) -> Option<Arc<Chunk>>;

    /// Current manifest snapshot. Returned by value so backings that hot-swap
    /// the manifest (live-edge polling) don't have to expose the underlying
    /// lock guard.
    fn manifest(&self) -> Arc<RrdManifest>;
    fn raw_manifest(&self) -> Arc<RawRrdManifest>;

    fn num_chunks(&self) -> usize;
    fn chunk_row_index(&self, id: &ChunkId) -> Option<usize>;

    /// Per-chunk timeline range, owned. Mirrors the lookup-style API used by
    /// `re_server` so callers don't need a borrow on the manifest's
    /// derived-index state.
    fn timeline_range(&self, chunk_id: &ChunkId) -> Option<IntMap<Timeline, AbsoluteTimeRange>>;

    fn extract_properties(&self) -> Result<RecordBatch, ExtractPropertiesError>;

    fn latest_at_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &LatestAtQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults;

    fn range_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &RangeQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults;

    fn load_chunks(&self, chunk_ids: &[ChunkId]) -> ChunkStoreResult<Vec<Arc<Chunk>>>;

    fn collect_physical_chunks(&self) -> ChunkStoreResult<Vec<Arc<Chunk>>>;

    /// Record the viewer's currently-visible time range on `timeline`.
    ///
    /// Implementations that don't run cursor-driven eviction (e.g. plain RRD
    /// files held entirely in memory) treat this as a no-op via the default
    /// implementation.
    fn observe_query_cursor(&self, timeline: TimelineName, range: AbsoluteTimeRange) {
        let _ = (timeline, range);
    }
}

impl LazyStore for crate::LazyRrdStore {
    fn store_id(&self) -> &StoreId {
        Self::store_id(self)
    }
    fn schema(&self) -> StoreSchema {
        Self::schema(self)
    }
    fn all_entities(&self) -> IntSet<EntityPath> {
        Self::all_entities(self)
    }
    fn physical_chunk(&self, id: &ChunkId) -> Option<Arc<Chunk>> {
        Self::physical_chunk(self, id)
    }
    fn manifest(&self) -> Arc<RrdManifest> {
        Self::manifest(self)
    }
    fn raw_manifest(&self) -> Arc<RawRrdManifest> {
        Self::raw_manifest(self)
    }
    fn num_chunks(&self) -> usize {
        Self::num_chunks(self)
    }
    fn chunk_row_index(&self, id: &ChunkId) -> Option<usize> {
        Self::chunk_row_index(self, id)
    }
    fn timeline_range(&self, chunk_id: &ChunkId) -> Option<IntMap<Timeline, AbsoluteTimeRange>> {
        Self::timeline_range(self, chunk_id)
    }
    fn extract_properties(&self) -> Result<RecordBatch, ExtractPropertiesError> {
        Self::extract_properties(self)
    }
    fn latest_at_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &LatestAtQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults {
        Self::latest_at_relevant_chunks_for_all_components(
            self,
            report_mode,
            query,
            entity_path,
            include_static,
        )
    }
    fn range_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &RangeQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults {
        Self::range_relevant_chunks_for_all_components(
            self,
            report_mode,
            query,
            entity_path,
            include_static,
        )
    }
    fn load_chunks(&self, chunk_ids: &[ChunkId]) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
        Self::load_chunks(self, chunk_ids)
    }
    fn collect_physical_chunks(&self) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
        Self::collect_physical_chunks(self)
    }
    // `LazyRrdStore` keeps the whole file in memory — no eviction, so cursor
    // observations are dropped on the floor (default impl).
}

impl<P: crate::ChunkProvider> LazyStore for crate::LazyChunkStore<P> {
    fn store_id(&self) -> &StoreId {
        Self::store_id(self)
    }
    fn schema(&self) -> StoreSchema {
        Self::schema(self)
    }
    fn all_entities(&self) -> IntSet<EntityPath> {
        Self::all_entities(self)
    }
    fn physical_chunk(&self, id: &ChunkId) -> Option<Arc<Chunk>> {
        Self::physical_chunk(self, id)
    }
    fn manifest(&self) -> Arc<RrdManifest> {
        Self::manifest(self)
    }
    fn raw_manifest(&self) -> Arc<RawRrdManifest> {
        Self::raw_manifest(self)
    }
    fn num_chunks(&self) -> usize {
        Self::num_chunks(self)
    }
    fn chunk_row_index(&self, id: &ChunkId) -> Option<usize> {
        Self::chunk_row_index(self, id)
    }
    fn timeline_range(&self, chunk_id: &ChunkId) -> Option<IntMap<Timeline, AbsoluteTimeRange>> {
        Self::timeline_range(self, chunk_id)
    }
    fn extract_properties(&self) -> Result<RecordBatch, ExtractPropertiesError> {
        Self::extract_properties(self)
    }
    fn latest_at_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &LatestAtQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults {
        Self::latest_at_relevant_chunks_for_all_components(
            self,
            report_mode,
            query,
            entity_path,
            include_static,
        )
    }
    fn range_relevant_chunks_for_all_components(
        &self,
        report_mode: ChunkTrackingMode,
        query: &RangeQuery,
        entity_path: &EntityPath,
        include_static: bool,
    ) -> QueryResults {
        Self::range_relevant_chunks_for_all_components(
            self,
            report_mode,
            query,
            entity_path,
            include_static,
        )
    }
    fn load_chunks(&self, chunk_ids: &[ChunkId]) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
        Self::load_chunks(self, chunk_ids)
    }
    fn collect_physical_chunks(&self) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
        Self::collect_physical_chunks(self)
    }
    fn observe_query_cursor(&self, timeline: TimelineName, range: AbsoluteTimeRange) {
        Self::observe_query_cursor(self, timeline, range);
    }
}
