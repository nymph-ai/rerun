use std::sync::Arc;

use futures::StreamExt as _;
use lance::Dataset;
use lance::dataset::builder::DatasetBuilder;
use parking_lot::Mutex;

use crate::chunk_row::{CorpusChunkRow, rows_from_record_batch};
use crate::config::CorpusConfig;
use crate::error::Result;

const PROJECTION: &[&str] = &[
    "chunk_id",
    "session_id",
    "room",
    "participant_identity",
    "track_id",
    "sequence_no",
    "chunk_start_ns",
    "chunk_end_ns",
    "s3_bucket",
    "s3_key",
    "byte_size",
    "sha256",
    "codec_audio",
];

/// Wrapper around a `lance::Dataset` that knows how to scan our index
/// schema into `CorpusChunkRow`s.
///
/// `Dataset` is itself `Send + Sync`. We hold it in a `Mutex` only to make
/// `refresh()` (drop + reopen) atomic against concurrent reads — for a
/// `provider.load_chunks(...)` request, only the manifest snapshot is read,
/// not the dataset.
pub struct LanceCorpusIndex {
    config: Arc<CorpusConfig>,
    dataset: Mutex<Option<Dataset>>,
}

impl LanceCorpusIndex {
    pub fn new(config: Arc<CorpusConfig>) -> Self {
        Self {
            config,
            dataset: Mutex::new(None),
        }
    }

    /// Open the dataset if it isn't already and return the cached handle.
    /// Repeated calls after the first reuse the same `Dataset`.
    pub async fn ensure_open(&self) -> Result<Dataset> {
        if let Some(ds) = self.dataset.lock().clone() {
            return Ok(ds);
        }
        let storage = self.config.lance_storage_options();
        let ds = DatasetBuilder::from_uri(&self.config.lance_table_uri)
            .with_storage_options(storage)
            .load()
            .await?;
        *self.dataset.lock() = Some(ds.clone());
        Ok(ds)
    }

    /// Drop the cached dataset so the next `ensure_open` re-reads the
    /// manifest. Live-edge polling calls this between scans so newly
    /// committed fragments become visible.
    pub fn refresh(&self) {
        *self.dataset.lock() = None;
    }

    /// Stream every row of the index, decoded into `CorpusChunkRow`.
    /// Filtering / time-window slicing isn't applied here — Plan A's
    /// manifest is "the whole corpus" and the viewer scrolls its
    /// timeline. The provider is responsible for not exceeding the
    /// `max_initial_rows` cap.
    pub async fn scan_all(&self) -> Result<Vec<CorpusChunkRow>> {
        self.scan_with_filter(None).await
    }

    /// Stream rows whose `chunk_start_ns` is strictly greater than
    /// `after_ns`. Used by live-edge polling to avoid re-decoding rows
    /// the provider has already absorbed. The dataset is `refresh()`-ed
    /// before the scan so freshly committed fragments are visible.
    pub async fn scan_after(&self, after_ns: i64) -> Result<Vec<CorpusChunkRow>> {
        self.refresh();
        self.scan_with_filter(Some(after_ns)).await
    }

    async fn scan_with_filter(&self, after_ns: Option<i64>) -> Result<Vec<CorpusChunkRow>> {
        let dataset = self.ensure_open().await?;
        let mut scanner = dataset.scan();
        scanner.project(PROJECTION)?;
        if let Some(after) = after_ns {
            // Lance's filter expression engine accepts a SQL-ish predicate.
            // `chunk_start_ns` is a non-null int64 column in the index
            // schema, so a strict `>` is enough — `chunk_start_ns` is
            // monotonic per track, and the watermark passed in is the
            // largest start the provider has already absorbed.
            scanner.filter(format!("chunk_start_ns > {after}").as_str())?;
        }
        if let Some(limit) = self.config.max_initial_rows {
            scanner.limit(Some(limit as i64), None)?;
        }

        let mut stream = scanner.try_into_stream().await?;
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            rows.extend(rows_from_record_batch(&batch)?);
        }
        // Sort by chunk_start_ns to keep timeline ranges monotonic. Lance
        // doesn't guarantee scan order; the manifest builder doesn't
        // require sorted input either, but a sorted manifest yields
        // tighter per-component time-range columns.
        rows.sort_by_key(|r| r.chunk_start_ns);
        Ok(rows)
    }
}
