use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;
use re_chunk::{Chunk, ChunkId};
use re_chunk_store::{ChunkProvider, ChunkStoreError, ChunkStoreResult};
use re_log_encoding::{RawRrdManifest, RrdManifest, RrdManifestBuilder};
use re_log_types::{StoreId, StoreKind};

use crate::chunk_builder::{build_segment_chunk, build_static_chunk};
use crate::chunk_row::CorpusChunkRow;
use crate::config::CorpusConfig;
use crate::error::Result;
use crate::index::LanceCorpusIndex;
use crate::opus_demux::demux_ogg_opus;
use crate::s3_fetch::S3Fetcher;

/// Maps a corpus row identifier to the rerun-side metadata we need to
/// rebuild the chunk on demand: `(row, chunk_id, kind)`.
#[derive(Debug, Clone)]
struct ManifestEntry {
    row: CorpusChunkRow,
    chunk_id: ChunkId,
    kind: ChunkKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkKind {
    Static,
    Segment,
}

/// Cumulative state the provider maintains across live-edge polls.
///
/// Wrapped in a single `RwLock` so a poll cycle that succeeds replaces
/// every field atomically — readers either see the pre-poll snapshot or
/// the post-poll snapshot, never a torn mix where the manifest has new
/// chunks but `entries` doesn't yet know how to materialize them.
struct ProviderState {
    raw_manifest: Arc<RawRrdManifest>,
    manifest: Arc<RrdManifest>,
    entries: HashMap<ChunkId, ManifestEntry>,
    /// Largest `chunk_start_ns` already absorbed. Live-edge polling uses
    /// this as the watermark for `LanceCorpusIndex::scan_after`. `None`
    /// means the manifest is empty and a `scan_all` is appropriate.
    watermark_ns: Option<i64>,
    /// Bumps on every successful absorb so polling tasks can detect "no
    /// new rows" without comparing manifests.
    revision: u64,
}

/// The actual provider. Built from a [`CorpusConfig`]; a single instance is
/// kept inside the `LazyChunkStore<LanceCorpusProvider>` for the process'
/// lifetime.
pub struct LanceCorpusProvider {
    config: Arc<CorpusConfig>,
    state: Arc<RwLock<ProviderState>>,
    runtime: Arc<tokio::runtime::Handle>,
    index: Arc<LanceCorpusIndex>,
    s3: Arc<S3Fetcher>,
}

impl LanceCorpusProvider {
    /// Build a provider by scanning the Lance index up front.
    ///
    /// `runtime` is a Tokio runtime handle used for the synchronous
    /// [`ChunkProvider::load_chunks`] surface — `block_on` runs on this
    /// handle so the calling thread (typically the chunk-store reader)
    /// doesn't need to be inside a Tokio context.
    pub async fn build(config: CorpusConfig, runtime: tokio::runtime::Handle) -> Result<Self> {
        config.validate()?;
        let config = Arc::new(config);

        let index = Arc::new(LanceCorpusIndex::new(config.clone()));
        let s3 = Arc::new(S3Fetcher::new(config.clone()));

        let rows = index.scan_all().await?;
        let initial = build_initial_state(&config, rows)?;

        Ok(Self {
            config,
            state: Arc::new(RwLock::new(initial)),
            runtime: Arc::new(runtime),
            index,
            s3,
        })
    }

    pub fn config(&self) -> &CorpusConfig {
        &self.config
    }

    /// The current revision counter. Bumps once per successful poll that
    /// absorbed at least one new row. Useful for tests / metrics.
    pub fn revision(&self) -> u64 {
        self.state.read().revision
    }

    fn lookup_entry(&self, chunk_id: ChunkId) -> Option<ManifestEntry> {
        self.state.read().entries.get(&chunk_id).cloned()
    }

    /// Re-scan the Lance index for rows newer than the current watermark
    /// and absorb them. Returns the new (manifest, raw_manifest) snapshot
    /// when at least one row was added, or `None` when nothing changed.
    ///
    /// Callers (typically the live-edge polling task in
    /// `nereid-corpus-server`) feed the returned snapshot into
    /// [`LazyChunkStore::extend_with_manifest`] so the chunk store's
    /// virtual index picks up the new chunks.
    pub async fn poll_for_new_rows(
        &self,
    ) -> Result<Option<(Arc<RrdManifest>, Arc<RawRrdManifest>)>> {
        let watermark = self.state.read().watermark_ns;
        let new_rows = match watermark {
            Some(after) => self.index.scan_after(after).await?,
            None => self.index.scan_all().await?,
        };
        if new_rows.is_empty() {
            return Ok(None);
        }

        let mut state = self.state.write();
        absorb_rows(&self.config, &mut state, new_rows)?;
        let manifest = Arc::clone(&state.manifest);
        let raw_manifest = Arc::clone(&state.raw_manifest);
        Ok(Some((manifest, raw_manifest)))
    }

    /// Materialize one chunk from its manifest entry. Routed through
    /// `block_on` so the sync ChunkProvider trait can call into async
    /// (object_store) code.
    fn materialize(&self, entry: &ManifestEntry) -> Result<Arc<Chunk>> {
        match entry.kind {
            ChunkKind::Static => {
                let chunk = build_static_chunk(&entry.row, entry.chunk_id)?;
                Ok(Arc::new(chunk))
            }
            ChunkKind::Segment => {
                let s3 = self.s3.clone();
                let row = entry.row.clone();
                let runtime = Arc::clone(&self.runtime);
                let bytes = tokio::task::block_in_place(move || {
                    runtime.block_on(async move {
                        s3.get_object(&row.s3_bucket, &row.s3_key, row.sha256.as_deref())
                            .await
                    })
                })?;
                let packets = demux_ogg_opus(&bytes)?;
                let chunk = build_segment_chunk(&entry.row, entry.chunk_id, packets)?;
                Ok(Arc::new(chunk))
            }
        }
    }
}

fn build_initial_state(config: &CorpusConfig, rows: Vec<CorpusChunkRow>) -> Result<ProviderState> {
    let mut state = ProviderState {
        raw_manifest: Arc::new(empty_raw_manifest(config)?),
        manifest: Arc::new(empty_manifest(config)?),
        entries: HashMap::new(),
        watermark_ns: None,
        revision: 0,
    };

    if !rows.is_empty() {
        // The provider's "no rows yet" state holds an empty manifest;
        // initial absorb mirrors the live-edge path so there's only one
        // code path to maintain.
        absorb_rows(config, &mut state, rows)?;
    }

    Ok(state)
}

/// Build an empty manifest with the configured store id. Used as the
/// "no rows yet" baseline before the first scan absorbs data.
fn empty_manifest(config: &CorpusConfig) -> Result<RrdManifest> {
    let raw = empty_raw_manifest(config)?;
    Ok(RrdManifest::try_new(&raw)?)
}

fn empty_raw_manifest(config: &CorpusConfig) -> Result<RawRrdManifest> {
    let store_id = StoreId::new(
        StoreKind::Recording,
        config.application_id.clone(),
        config.recording_id.clone(),
    );
    let builder = RrdManifestBuilder::default();
    Ok(builder.build(store_id)?)
}

/// Append `new_rows` into `state`, rebuilding the manifest by replaying
/// every entry (existing + new). Lance's row-id schema is append-only —
/// rebuilding from `state.entries` plus `new_rows` is cheap relative to
/// the actual scan, and avoids any cross-poll byte-cursor bookkeeping.
fn absorb_rows(
    config: &CorpusConfig,
    state: &mut ProviderState,
    new_rows: Vec<CorpusChunkRow>,
) -> Result<()> {
    let store_id = StoreId::new(
        StoreKind::Recording,
        config.application_id.clone(),
        config.recording_id.clone(),
    );
    let mut builder = RrdManifestBuilder::default();
    let mut entries: HashMap<ChunkId, ManifestEntry> =
        HashMap::with_capacity(state.entries.len() + new_rows.len() * 2);
    let mut seen_entities: HashSet<String> = HashSet::new();
    let mut byte_cursor: u64 = 0;
    let mut absorbed_any = false;
    let mut new_watermark = state.watermark_ns;

    // Replay existing rows in chunk_start_ns order so the manifest's
    // per-component time-range columns stay tight. Static priming chunks
    // are emitted whenever a new entity first appears, just like the
    // initial scan.
    let mut all_rows: Vec<CorpusChunkRow> = state
        .entries
        .values()
        .filter_map(|entry| match entry.kind {
            ChunkKind::Segment => Some(entry.row.clone()),
            ChunkKind::Static => None,
        })
        .collect();
    all_rows.extend(new_rows.iter().cloned());
    all_rows.sort_by_key(|r| r.chunk_start_ns);
    // Dedupe by chunk_id_str — a row that arrives in `new_rows` and is
    // also already in `state.entries` (defensive: scan_after should
    // never produce one) collapses to a single manifest entry.
    all_rows.dedup_by(|a, b| a.chunk_id_str == b.chunk_id_str);

    for row in &all_rows {
        let entity_key = row.entity_path().to_string();
        if !seen_entities.contains(&entity_key) {
            let static_chunk_id = derive_static_chunk_id(&entity_key);
            let chunk = build_static_chunk(row, static_chunk_id)?;
            let batch = chunk.to_chunk_batch()?;
            let size = MANIFEST_DUMMY_SIZE;
            let span = re_chunk::Span {
                start: byte_cursor,
                len: size,
            };
            byte_cursor += size;
            builder.append(&batch, span, size)?;

            entries.insert(
                static_chunk_id,
                ManifestEntry {
                    row: row.clone(),
                    chunk_id: static_chunk_id,
                    kind: ChunkKind::Static,
                },
            );
            seen_entities.insert(entity_key.clone());
        }

        let segment_chunk_id = row.chunk_id();
        let placeholder = build_placeholder_segment(row, segment_chunk_id)?;
        let batch = placeholder.to_chunk_batch()?;
        let size = MANIFEST_DUMMY_SIZE;
        let span = re_chunk::Span {
            start: byte_cursor,
            len: size,
        };
        byte_cursor += size;
        builder.append(&batch, span, size)?;

        let was_new = !state.entries.contains_key(&segment_chunk_id);
        entries.insert(
            segment_chunk_id,
            ManifestEntry {
                row: row.clone(),
                chunk_id: segment_chunk_id,
                kind: ChunkKind::Segment,
            },
        );
        if was_new {
            absorbed_any = true;
            new_watermark = Some(
                new_watermark
                    .map(|w| w.max(row.chunk_start_ns))
                    .unwrap_or(row.chunk_start_ns),
            );
        }
    }

    let raw = builder.build(store_id)?;
    let manifest = RrdManifest::try_new(&raw)?;

    state.raw_manifest = Arc::new(raw);
    state.manifest = Arc::new(manifest);
    state.entries = entries;
    state.watermark_ns = new_watermark;
    if absorbed_any {
        state.revision = state.revision.wrapping_add(1);
    }
    Ok(())
}

/// Build a 1-row placeholder chunk that establishes the segment's timeline
/// span without fetching real Opus bytes. The manifest only needs to know
/// the entity, the timeline range, and which components exist — not the
/// payload — so we pass a single-byte AudioChunk and the row's known
/// `[chunk_start_ns, chunk_end_ns]` window.
fn build_placeholder_segment(row: &CorpusChunkRow, chunk_id: ChunkId) -> Result<Chunk> {
    use re_chunk::{RowId, TimePoint};
    use re_log_types::{TimeCell, Timeline};
    use re_sdk_types::archetypes::AudioStream;
    use re_sdk_types::components::{AudioChunk, AudioDurationSamples, AudioSequenceNumber};

    let entity_path = row.entity_path();
    let timeline = Timeline::new_timestamp(crate::chunk_builder::CAPTURE_TIMELINE);

    // Two synthetic rows at the start and end of the segment so the
    // manifest's per-component time-range column covers `[start, end)`.
    let archetype_start = AudioStream::update_fields()
        .with_chunk(AudioChunk::from(vec![0u8]))
        .with_duration_samples(AudioDurationSamples::from(0u64))
        .with_sequence_number(AudioSequenceNumber::from(
            row.sequence_no as u64 * 1_000_000,
        ));
    let archetype_end = AudioStream::update_fields()
        .with_chunk(AudioChunk::from(vec![0u8]))
        .with_duration_samples(AudioDurationSamples::from(0u64))
        .with_sequence_number(AudioSequenceNumber::from(
            row.sequence_no as u64 * 1_000_000 + 1,
        ));

    let tp_start = TimePoint::default().with(
        timeline,
        TimeCell::from_timestamp_nanos_since_epoch(row.chunk_start_ns),
    );
    let tp_end = TimePoint::default().with(
        timeline,
        TimeCell::from_timestamp_nanos_since_epoch(row.chunk_end_ns.max(row.chunk_start_ns + 1)),
    );

    let chunk = Chunk::builder_with_id(chunk_id, entity_path)
        .with_archetype(RowId::new(), tp_start, &archetype_start)
        .with_archetype(RowId::new(), tp_end, &archetype_end)
        .build()?;
    Ok(chunk)
}

fn derive_static_chunk_id(entity_path: &str) -> ChunkId {
    use sha2::{Digest as _, Sha256};
    let key = format!("static::{entity_path}");
    let digest = Sha256::digest(key.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ChunkId::from_u128(u128::from_be_bytes(bytes))
}

impl ChunkProvider for LanceCorpusProvider {
    fn manifest(&self) -> Arc<RrdManifest> {
        Arc::clone(&self.state.read().manifest)
    }

    fn raw_manifest(&self) -> Arc<RawRrdManifest> {
        Arc::clone(&self.state.read().raw_manifest)
    }

    fn load_chunks(&self, chunk_ids: &[ChunkId]) -> ChunkStoreResult<Vec<Arc<Chunk>>> {
        let mut out = Vec::with_capacity(chunk_ids.len());
        for &id in chunk_ids {
            let entry = self.lookup_entry(id).ok_or_else(|| {
                ChunkStoreError::Codec(re_log_encoding::CodecError::ChunkNotInManifest {
                    chunk_id: id,
                })
            })?;
            let chunk = self.materialize(&entry).map_err(|e| {
                let err: ChunkStoreError = e.into();
                err
            })?;
            out.push(chunk);
        }
        Ok(out)
    }
}

/// Manifest entries record a non-zero byte size so [`RrdManifestBuilder`]
/// produces well-formed `byte_offset`/`byte_size` columns. The actual
/// values are unused — the provider looks chunks up by `ChunkId`, never
/// by offset — so a fixed sentinel is fine.
const MANIFEST_DUMMY_SIZE: u64 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_row(chunk_id_str: &str, sequence_no: i64, start_ns: i64) -> CorpusChunkRow {
        CorpusChunkRow {
            chunk_id_str: chunk_id_str.to_owned(),
            room: "room1".into(),
            participant_identity: "p1".into(),
            track_id: "trk1".into(),
            sequence_no,
            chunk_start_ns: start_ns,
            // 20-ms windows; the absolute value isn't load-bearing for these
            // assertions, only that end > start so the placeholder builder
            // doesn't rebase the upper bound itself.
            chunk_end_ns: start_ns + 20_000_000,
            s3_bucket: "bucket".into(),
            s3_key: format!("k/{chunk_id_str}.opus"),
            sha256: None,
            codec_audio: Some("opus".into()),
        }
    }

    fn test_config() -> CorpusConfig {
        CorpusConfig {
            lance_table_uri: "s3://test/index.lance".into(),
            s3_endpoint: String::new(),
            s3_region: "us-east-1".into(),
            s3_access_key: String::new(),
            s3_secret_key: String::new(),
            s3_force_path_style: true,
            application_id: "test_app".into(),
            recording_id: "test_rec".into(),
            max_initial_rows: None,
        }
    }

    #[test]
    fn absorb_rows_extends_manifest_and_advances_watermark() {
        let config = test_config();
        let initial = vec![synthetic_row("c1", 1, 1_000_000_000)];
        let mut state = build_initial_state(&config, initial).expect("initial state");

        // One static priming chunk + one segment chunk for the first row.
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.watermark_ns, Some(1_000_000_000));
        let rev_after_init = state.revision;

        // Absorbing a strictly-newer row should add exactly one segment
        // entry (no new entity → no new static), and advance the watermark.
        let new_row = synthetic_row("c2", 2, 2_000_000_000);
        absorb_rows(&config, &mut state, vec![new_row]).expect("absorb");

        assert_eq!(state.entries.len(), 3);
        assert_eq!(state.watermark_ns, Some(2_000_000_000));
        assert_eq!(state.revision, rev_after_init.wrapping_add(1));

        // A redundant absorb of the *same* row must not bump the revision
        // (live-edge polling occasionally re-sees rows after refresh races).
        absorb_rows(
            &config,
            &mut state,
            vec![synthetic_row("c2", 2, 2_000_000_000)],
        )
        .expect("redundant absorb");
        assert_eq!(state.entries.len(), 3);
        assert_eq!(state.revision, rev_after_init.wrapping_add(1));
    }
}
