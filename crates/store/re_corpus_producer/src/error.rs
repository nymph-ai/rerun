use re_chunk_store::ChunkStoreError;

#[derive(thiserror::Error, Debug)]
pub enum CorpusError {
    #[error("lance: {0}")]
    Lance(#[from] lance::Error),

    #[error("object_store: {0}")]
    ObjectStore(#[from] object_store::Error),

    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),

    #[error("chunk: {0}")]
    Chunk(#[from] re_chunk::ChunkError),

    #[error("codec: {0}")]
    Codec(#[from] re_log_encoding::CodecError),

    #[error("chunk_store: {0}")]
    ChunkStore(#[from] ChunkStoreError),

    #[error("url parse: {0}")]
    Url(#[from] url::ParseError),

    #[error("config: {0}")]
    Config(String),

    #[error("missing column `{0}` in lance scan output")]
    MissingColumn(&'static str),

    #[error("row {row} column `{column}` is null")]
    NullCell { row: usize, column: &'static str },

    #[error("chunk `{chunk_id}` not in manifest")]
    UnknownChunk { chunk_id: String },

    #[error("sha256 mismatch for {bucket}/{key}: expected={expected} actual={actual}")]
    Sha256Mismatch {
        bucket: String,
        key: String,
        expected: String,
        actual: String,
    },

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, CorpusError>;

impl From<CorpusError> for ChunkStoreError {
    fn from(value: CorpusError) -> Self {
        match value {
            CorpusError::Codec(err) => Self::Codec(err),
            CorpusError::Chunk(err) => Self::Chunk(err),
            CorpusError::ChunkStore(err) => err,
            other => Self::Codec(re_log_encoding::CodecError::Io(std::io::Error::other(
                other.to_string(),
            ))),
        }
    }
}
