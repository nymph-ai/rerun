use arrow::array::{Array, Int64Array, RecordBatch, StringArray};
use sha2::{Digest as _, Sha256};

use re_chunk::ChunkId;

use crate::error::{CorpusError, Result};

/// One row of the corpus index materialized into the fields the producer
/// needs to construct a Rerun chunk + load its Opus blob.
#[derive(Debug, Clone)]
pub struct CorpusChunkRow {
    pub chunk_id_str: String,
    pub room: String,
    pub participant_identity: String,
    pub track_id: String,
    pub sequence_no: i64,
    pub chunk_start_ns: i64,
    pub chunk_end_ns: i64,
    pub s3_bucket: String,
    pub s3_key: String,
    pub sha256: Option<String>,
    /// Codec identifier from the index row. Used in Phase 5 to validate
    /// that a row matches the rerun-side `AudioCodec::Opus` assumption
    /// before we attempt to demux it.
    #[allow(dead_code)]
    pub codec_audio: Option<String>,
}

impl CorpusChunkRow {
    /// Stable [`ChunkId`] derived from the corpus chunk identifier string.
    /// Stability across restarts is critical: the viewer caches by
    /// `ChunkId`, so a new ID for the same row would fool it into thinking
    /// the data changed.
    pub fn chunk_id(&self) -> ChunkId {
        derive_chunk_id(&self.chunk_id_str)
    }

    /// `EntityPath` mirroring the layout used by the python materializer:
    /// `/corpus/{room}/{participant}/{track}/audio`.
    pub fn entity_path(&self) -> re_log_types::EntityPath {
        re_log_types::EntityPath::from(format!(
            "/corpus/{}/{}/{}/audio",
            slug(&self.room),
            slug(&self.participant_identity),
            slug(&self.track_id),
        ))
    }

    pub fn static_entity_path(&self) -> re_log_types::EntityPath {
        // Same path as the per-row entity; codec/sample_rate/channel_count
        // are logged static against it.
        self.entity_path()
    }
}

fn derive_chunk_id(chunk_id_str: &str) -> ChunkId {
    let digest = Sha256::digest(chunk_id_str.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ChunkId::from_u128(u128::from_be_bytes(bytes))
}

fn slug(value: &str) -> String {
    let s = value.replace('/', "_");
    if s.is_empty() {
        "unknown".to_owned()
    } else {
        s
    }
}

/// Decode `CorpusChunkRow`s from a record batch returned by the lance
/// scanner. Required columns (chunk_id, room, participant_identity,
/// track_id, sequence_no, chunk_start_ns, chunk_end_ns, s3_bucket, s3_key)
/// must be present; sha256 / codec_audio are optional.
pub fn rows_from_record_batch(batch: &RecordBatch) -> Result<Vec<CorpusChunkRow>> {
    let chunk_id = required_string(batch, "chunk_id")?;
    let room = required_string(batch, "room")?;
    let participant = required_string(batch, "participant_identity")?;
    let track_id = required_string(batch, "track_id")?;
    let sequence_no = required_int64(batch, "sequence_no")?;
    let chunk_start_ns = required_int64(batch, "chunk_start_ns")?;
    let chunk_end_ns = required_int64(batch, "chunk_end_ns")?;
    let s3_bucket = required_string(batch, "s3_bucket")?;
    let s3_key = required_string(batch, "s3_key")?;
    let sha256 = optional_string(batch, "sha256");
    let codec_audio = optional_string(batch, "codec_audio");

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        if chunk_id.is_null(i) {
            return Err(CorpusError::NullCell {
                row: i,
                column: "chunk_id",
            });
        }
        let cid = chunk_id.value(i).to_owned();
        let bucket = s3_bucket.value(i).to_owned();
        let key = s3_key.value(i).to_owned();
        if bucket.is_empty() || key.is_empty() {
            // Skip rows where the chunk publish hasn't landed yet
            // (transcript_status NOT NULL but s3_* NULL is allowed by the
            // schema as a transient state). The streamer should only
            // surface fully-published chunks.
            continue;
        }
        out.push(CorpusChunkRow {
            chunk_id_str: cid,
            room: room.value(i).to_owned(),
            participant_identity: participant.value(i).to_owned(),
            track_id: track_id.value(i).to_owned(),
            sequence_no: sequence_no.value(i),
            chunk_start_ns: chunk_start_ns.value(i),
            chunk_end_ns: chunk_end_ns.value(i),
            s3_bucket: bucket,
            s3_key: key,
            sha256: sha256.as_ref().and_then(|c| nullable_str(c, i)),
            codec_audio: codec_audio.as_ref().and_then(|c| nullable_str(c, i)),
        });
    }
    Ok(out)
}

fn required_string<'a>(batch: &'a RecordBatch, column: &'static str) -> Result<&'a StringArray> {
    let col = batch
        .column_by_name(column)
        .ok_or(CorpusError::MissingColumn(column))?;
    col.as_any()
        .downcast_ref::<StringArray>()
        .ok_or(CorpusError::MissingColumn(column))
}

fn required_int64<'a>(batch: &'a RecordBatch, column: &'static str) -> Result<&'a Int64Array> {
    let col = batch
        .column_by_name(column)
        .ok_or(CorpusError::MissingColumn(column))?;
    col.as_any()
        .downcast_ref::<Int64Array>()
        .ok_or(CorpusError::MissingColumn(column))
}

fn optional_string<'a>(batch: &'a RecordBatch, column: &'static str) -> Option<&'a StringArray> {
    batch
        .column_by_name(column)?
        .as_any()
        .downcast_ref::<StringArray>()
}

fn nullable_str(arr: &StringArray, i: usize) -> Option<String> {
    if arr.is_null(i) {
        None
    } else {
        let s = arr.value(i);
        if s.is_empty() {
            None
        } else {
            Some(s.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_id_is_stable_across_calls() {
        let a = derive_chunk_id("abc-123");
        let b = derive_chunk_id("abc-123");
        let c = derive_chunk_id("abc-124");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn entity_path_slugs_slashes() {
        let row = CorpusChunkRow {
            chunk_id_str: "x".into(),
            room: "room/with/slashes".into(),
            participant_identity: "p1".into(),
            track_id: "t1".into(),
            sequence_no: 0,
            chunk_start_ns: 0,
            chunk_end_ns: 0,
            s3_bucket: "b".into(),
            s3_key: "k".into(),
            sha256: None,
            codec_audio: None,
        };
        assert_eq!(
            row.entity_path().to_string(),
            "/corpus/room_with_slashes/p1/t1/audio"
        );
    }
}
