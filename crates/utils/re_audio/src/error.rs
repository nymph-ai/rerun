use thiserror::Error;

/// Top-level error returned by `re_audio` APIs.
#[derive(Error, Debug)]
pub enum AudioError {
    /// The codec used by the stream is not supported by the current build.
    #[error("unsupported audio codec: {0}")]
    UnsupportedCodec(&'static str),

    /// A decoder-level error (bad packet, libopus failure, `WebCodecs` callback).
    #[error(transparent)]
    Decode(#[from] DecodeError),

    /// The segment index has no seekable boundary at or before the requested time.
    #[error("no seekable segment at or before pts {0} ns")]
    NoSeekableBoundary(i64),

    /// Requested to mix/resample between incompatible PCM formats.
    #[error("channel count mismatch: decoder produced {got}, expected {expected}")]
    ChannelMismatch {
        /// Channels produced by the decoder.
        got: u16,
        /// Channels expected by the pipeline.
        expected: u16,
    },
}

/// Decoder-level error surface.
#[derive(Error, Debug, Clone)]
pub enum DecodeError {
    /// A libopus or `WebCodecs` call failed with a message.
    #[error("decoder failed: {0}")]
    Backend(String),

    /// The encoded chunk was empty or malformed.
    #[error("malformed encoded chunk")]
    BadChunk,

    /// The decoder was used before being configured.
    #[error("decoder not configured")]
    NotConfigured,
}
