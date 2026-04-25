//! Construct Rerun chunks from corpus rows + Opus payloads.
//!
//! Two flavors of chunk:
//!   * **Static priming chunk** — codec/sample_rate/channel_count, one per
//!     audio entity, built once at startup so the manifest already advertises
//!     those static fields.
//!   * **Per-row segment chunk** — for each lance row, one Rerun chunk
//!     containing every Opus packet found inside the OGG bytes, each at its
//!     own PTS-derived timestamp on the `capture_time` timeline.

use re_chunk::{Chunk, ChunkId, ChunkResult, RowId, TimePoint};
use re_log_types::{TimeCell, Timeline};
use re_sdk_types::archetypes::AudioStream;
use re_sdk_types::components::{AudioChunk, AudioCodec, AudioDurationSamples, AudioSequenceNumber};

use crate::chunk_row::CorpusChunkRow;

/// Timeline used for absolute wall-clock playback of the corpus.
pub const CAPTURE_TIMELINE: &str = "capture_time";

/// Build the static priming chunk for a single audio entity.
///
/// This advertises `codec=Opus`, `sample_rate=48000`, `channel_count=1`
/// (the LiveKit TrackEgress shape — matches `RrdMaterializer._prime_static`
/// in nereid/corpus_streamer/rerun_sink.py).
pub fn build_static_chunk(row: &CorpusChunkRow, chunk_id: ChunkId) -> ChunkResult<Chunk> {
    let entity_path = row.static_entity_path();
    let archetype = AudioStream::new(AudioCodec::Opus, 48_000_u32, 1_u16);
    Chunk::builder_with_id(chunk_id, entity_path)
        .with_archetype(RowId::new(), TimePoint::default(), &archetype)
        .build()
}

/// Build a per-segment chunk from one corpus row + the OGG bytes fetched
/// from S3.
///
/// `packets` is the list of decoded Opus packets in PTS order. The caller
/// (the provider) is responsible for OGG demuxing — we keep this fn
/// codec-aware enough to populate the right components but free of the
/// libav dep.
pub fn build_segment_chunk(
    row: &CorpusChunkRow,
    chunk_id: ChunkId,
    packets: Vec<OpusPacket>,
) -> ChunkResult<Chunk> {
    let entity_path = row.entity_path();
    let mut builder = Chunk::builder_with_id(chunk_id, entity_path);
    let timeline = Timeline::new_timestamp(CAPTURE_TIMELINE);

    for (i, packet) in packets.iter().enumerate() {
        let pts_ns = row.chunk_start_ns.saturating_add(packet.pts_ns);
        let timepoint = TimePoint::default()
            .with(timeline, TimeCell::from_timestamp_nanos_since_epoch(pts_ns));

        let archetype = AudioStream::update_fields()
            .with_chunk(AudioChunk::from(packet.bytes.clone()))
            .with_duration_samples(AudioDurationSamples::from(packet.duration_samples as u64))
            .with_sequence_number(AudioSequenceNumber::from(
                row.sequence_no as u64 * 1_000_000 + i as u64,
            ));

        builder = builder.with_archetype(RowId::new(), timepoint, &archetype);
    }

    builder.build()
}

/// One demuxed Opus packet ready to log as a single row inside the
/// segment chunk.
#[derive(Debug, Clone)]
pub struct OpusPacket {
    /// Raw Opus packet bytes (no OGG framing).
    pub bytes: Vec<u8>,
    /// Presentation time, nanoseconds from the start of the segment.
    pub pts_ns: i64,
    /// Number of PCM samples per channel that decoding this packet produces.
    pub duration_samples: i64,
}
