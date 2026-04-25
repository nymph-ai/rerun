//! A backing-store abstraction for [`LazyChunkStore`].
//!
//! Provides a generic substrate for "manifest known up front, chunks loaded on
//! demand" stores. The canonical implementation [`crate::LazyRrdStore`] is
//! backed by an RRD file; the trait lets other backings (Lance tables, S3
//! object stores, etc.) plug in without re-implementing the autoload plumbing.
//!
//! This addresses upstream `TODO(RR-4341)` on `LazyRrdStore`.

use std::sync::Arc;

use re_chunk::{Chunk, ChunkId};
use re_log_encoding::{RawRrdManifest, RrdManifest};

use crate::ChunkStoreResult;

/// A backing source for a [`LazyChunkStore`].
///
/// Implementors expose:
///   * A pre-built [`RrdManifest`] enumerating all known chunks (the *virtual*
///     set). The manifest populates the chunk store's index up front.
///   * A `load_chunks` method that materializes specific chunks on demand,
///     returning the decoded [`Chunk`]s. Already-loaded chunks may be filtered
///     by the caller; implementors should be tolerant of redundant requests.
///
/// Implementors must be `Send + Sync` because a [`LazyChunkStore`] is shared
/// across threads via `Arc`.
pub trait ChunkProvider: Send + Sync + 'static {
    /// The validated manifest. Populates the chunk store's virtual index on
    /// construction and is consulted on every chunk lookup.
    ///
    /// Returns an owned `Arc` so providers backed by mutable state (live-edge
    /// polling, append-only Lance indexes) can swap the snapshot internally
    /// without breaking the borrow checker.
    fn manifest(&self) -> Arc<RrdManifest>;

    /// The raw manifest as parsed from the underlying source.
    ///
    /// Required because [`crate::LazyChunkStore::raw_manifest`] is consumed by
    /// `re_server` to synthesize `GetRrdManifest` responses without
    /// materializing chunks. Backings that synthesize a manifest from external
    /// state (e.g. a Lance index) build a [`RawRrdManifest`] via
    /// [`re_log_encoding::RrdManifestBuilder`] and return it here.
    fn raw_manifest(&self) -> Arc<RawRrdManifest>;

    /// Materialize a set of chunks by ID.
    ///
    /// Implementors should:
    ///   * Return only chunks that were not already in the store (the caller
    ///     filters, but a defensive provider may also dedupe).
    ///   * Return [`crate::ChunkStoreError::Codec`] /
    ///     [`re_log_encoding::CodecError::ChunkNotInManifest`] for unknown IDs.
    ///   * Avoid blocking on shared store locks — `LazyChunkStore::load_chunks`
    ///     calls this without any guard held and inserts results afterwards.
    fn load_chunks(&self, chunk_ids: &[ChunkId]) -> ChunkStoreResult<Vec<Arc<Chunk>>>;
}
