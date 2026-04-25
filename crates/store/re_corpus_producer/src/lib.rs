//! `ChunkProvider` implementation backed by a Lance corpus index + S3
//! object storage holding per-track Opus chunks.
//!
//! See `README.md` for the high-level design. The crate exposes:
//!   * [`CorpusConfig`] — Lance URI, S3 credentials, recording id.
//!   * [`LanceCorpusProvider`] — implements
//!     [`re_chunk_store::ChunkProvider`] so it can back a
//!     [`re_chunk_store::LazyChunkStore`].
//!   * [`CorpusError`] — crate-level error type that converts into the
//!     `ChunkStoreError` exposed through the trait.

mod chunk_builder;
mod chunk_row;
mod config;
mod error;
mod index;
mod opus_demux;
mod provider;
mod s3_fetch;

pub use chunk_builder::CAPTURE_TIMELINE;
pub use config::CorpusConfig;
pub use error::{CorpusError, Result};
pub use provider::LanceCorpusProvider;
