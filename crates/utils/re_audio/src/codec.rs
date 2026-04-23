//! Codec and channel-layout descriptors used throughout `re_audio`.
//!
//! The values here mirror the on-the-wire component representations
//! (`AudioCodec`, `AudioChannelLayout`) so that viewer glue can convert in
//! one trivial `match` without pulling `re_sdk_types` into this crate.

/// Kinds of audio codecs known to the decoder registry.
///
/// The discriminant matches the `WebCodecs` / `FourCC` convention used by
/// `components::AudioCodec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum AudioCodecKind {
    /// Opus — RFC 6716.
    Opus = 0x6f70_7573,
    /// Free Lossless Audio Codec.
    Flac = 0x666c_6163,
}

impl AudioCodecKind {
    /// Attempt to construct from the raw `FourCC` value logged in a chunk.
    pub fn from_fourcc(value: u32) -> Option<Self> {
        match value {
            0x6f70_7573 => Some(Self::Opus),
            0x666c_6163 => Some(Self::Flac),
            _ => None,
        }
    }

    /// The `WebCodecs` codec string (e.g. "opus").
    pub fn webcodecs_string(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Flac => "flac",
        }
    }

    /// Short human-readable name, suitable for UI badges.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Flac => "FLAC",
        }
    }
}

/// Channel layouts recognized by the player.
///
/// These mirror `components::AudioChannelLayout`. The integer value equals
/// the channel count for layouts whose name is unambiguous; layouts with
/// ambiguous counts (e.g. ambisonic) use dedicated values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum ChannelLayout {
    /// Unknown or codec-reported layout. The viewer treats channels as raw
    /// interleaved. This mirrors the `Invalid` variant in the on-the-wire
    /// component but is kept visible in the Rust API so callers can reason
    /// about "unknown" explicitly.
    Unknown = 0,
    /// Single channel.
    Mono = 1,
    /// L, R.
    Stereo = 2,
    /// L, R, C.
    Lcr = 3,
    /// L, R, LS, RS.
    Quad = 4,
    /// L, R, C, LS, RS (no LFE).
    FiveDot = 5,
    /// L, R, C, LFE, LS, RS.
    FiveDotOne = 6,
    /// L, R, C, LFE, LS, RS, SL, SR.
    SevenDotOne = 8,
    /// First-order ambisonic (4 channels).
    Ambisonic1stOrder = 10,
}

impl ChannelLayout {
    /// Number of interleaved channels this layout produces, or `None` for
    /// layouts that cannot be inferred without side information (e.g.
    /// `Unspecified`).
    pub fn channel_count(self) -> Option<u16> {
        match self {
            Self::Unknown => None,
            Self::Mono => Some(1),
            Self::Stereo => Some(2),
            Self::Lcr => Some(3),
            Self::Quad | Self::Ambisonic1stOrder => Some(4),
            Self::FiveDot => Some(5),
            Self::FiveDotOne => Some(6),
            Self::SevenDotOne => Some(8),
        }
    }
}
