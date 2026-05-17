//! Demux raw Opus packets out of OGG container bytes.
//!
//! The corpus's chunker (LiveKit TrackEgress → websocket → chunker) emits
//! one `.ogg` blob per chunk. Rerun's `AudioStream` archetype with
//! `AudioCodec::Opus` expects raw Opus packets (not OGG-framed bytes), so
//! we demux here.
//!
//! Two OGG packets at the start of every Opus stream are headers
//! (`OpusHead`, `OpusTags`) — these aren't decodable audio and must be
//! dropped. From there, every subsequent packet is one Opus packet.
//!
//! PTS calculation:
//!   * The OGG `granule_position` for Opus = total decoded sample count
//!     (at 48 kHz) at the end of the page.
//!   * Per-packet duration is computed from the Opus TOC byte (config +
//!     frame_count) — robust to non-uniform frame sizes.
//!   * `pts_ns_in_segment` = cumulative samples before this packet, in ns.

use std::io::Cursor;

use ogg::reading::PacketReader;

use crate::chunk_builder::OpusPacket;
use crate::error::{CorpusError, Result};

/// Sample rate for Opus PTS arithmetic. Opus is always operated at 48 kHz
/// at the OGG layer regardless of the sample rate the encoder was set to.
const OPUS_OGG_RATE: u64 = 48_000;

pub fn demux_ogg_opus(ogg_bytes: &[u8]) -> Result<Vec<OpusPacket>> {
    let cursor = Cursor::new(ogg_bytes);
    let mut reader = PacketReader::new(cursor);
    let mut packets = Vec::new();
    let mut header_packets_seen = 0usize;
    let mut cumulative_samples: u64 = 0;

    while let Some(packet) = reader
        .read_packet()
        .map_err(|e| CorpusError::Internal(format!("ogg demux: {e}")))?
    {
        // Skip OpusHead + OpusTags.
        if header_packets_seen < 2 {
            header_packets_seen += 1;
            continue;
        }

        let duration_samples = opus_packet_duration_samples(&packet.data)
            .ok_or_else(|| CorpusError::Internal("invalid Opus packet TOC".to_owned()))?;

        let pts_samples = cumulative_samples;
        cumulative_samples = cumulative_samples.saturating_add(duration_samples);

        let pts_ns = (pts_samples as i64).saturating_mul(1_000_000_000) / OPUS_OGG_RATE as i64;

        packets.push(OpusPacket {
            bytes: packet.data,
            pts_ns,
            duration_samples: duration_samples as i64,
        });
    }

    Ok(packets)
}

/// Decode the duration of a single Opus packet from its TOC byte.
///
/// Opus packet structure (RFC 6716 §3.1):
///   * Byte 0 = TOC: `[config:5][s:1][c:2]`
///     * `config` (0..31) selects (mode, bandwidth, frame_size)
///     * `c` is the frame-count code
///       * `c=0`: 1 frame
///       * `c=1`: 2 frames (equal duration)
///       * `c=2`: 2 frames (different duration; signalling in subsequent bytes)
///       * `c=3`: arbitrary frames; first 2 bits of byte 1 give the count
///
/// Frame size in samples (at 48 kHz) per `config`:
///   * 0..11   →  120 / 240 / 480 / 960     (SILK   NB/MB/WB/SWB at various sizes)
///   * 12..15  →  480 / 960                  (Hybrid SWB/FB)
///   * 16..19  →  120 / 240 / 480 / 960     (CELT NB)
///   * 20..23  →  120 / 240 / 480 / 960     (CELT WB)
///   * 24..27  →  120 / 240 / 480 / 960     (CELT SWB)
///   * 28..31  →  120 / 240 / 480 / 960     (CELT FB)
///
/// The size pattern within each band is `[2.5ms, 5ms, 10ms, 20ms]`, which at
/// 48 kHz is `[120, 240, 480, 960]`. SILK has different sizes (10/20/40/60 ms
/// → 480/960/1920/2880).
fn opus_packet_duration_samples(packet: &[u8]) -> Option<u64> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let c = toc & 0b11;

    let frame_size_48k = opus_frame_size_samples(config)?;
    let frame_count: u64 = match c {
        0 => 1,
        1 | 2 => 2,
        3 => {
            // Frame count is the low 6 bits of byte 1 (mask 0x3F). 0 is
            // invalid per RFC. We're tolerant of "odd but parseable"
            // packets; on truly invalid TOCs we punt to caller.
            let b1 = *packet.get(1)?;
            let n = (b1 & 0x3F) as u64;
            if n == 0 {
                return None;
            }
            n
        }
        _ => unreachable!(),
    };

    Some(frame_size_48k * frame_count)
}

fn opus_frame_size_samples(config: u8) -> Option<u64> {
    // SILK NB/MB/WB
    let size = match config {
        // SILK NB/MB/WB: 10/20/40/60 ms
        0..=2 => match config % 4 {
            0 => 480,
            1 => 960,
            2 => 1920,
            _ => 2880,
        },
        3 => 2880,
        4..=7 => match config % 4 {
            0 => 480,
            1 => 960,
            2 => 1920,
            _ => 2880,
        },
        8..=11 => match config % 4 {
            0 => 480,
            1 => 960,
            2 => 1920,
            _ => 2880,
        },
        // Hybrid SWB/FB: 10/20 ms
        12..=13 => match config % 2 {
            0 => 480,
            _ => 960,
        },
        14..=15 => match config % 2 {
            0 => 480,
            _ => 960,
        },
        // CELT NB/WB/SWB/FB: 2.5/5/10/20 ms
        16..=31 => match config % 4 {
            0 => 120,
            1 => 240,
            2 => 480,
            _ => 960,
        },
        _ => return None,
    };
    Some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_celt_fb_20ms() {
        // config=31 → CELT FB 20 ms = 960 samples (last of CELT FB group)
        assert_eq!(opus_frame_size_samples(31), Some(960));
        // config=23 → CELT WB 20 ms = 960 samples
        assert_eq!(opus_frame_size_samples(23), Some(960));
    }

    #[test]
    fn frame_size_silk_60ms() {
        // SILK NB 60 ms = 2880 samples
        assert_eq!(opus_frame_size_samples(3), Some(2880));
    }

    #[test]
    fn frame_size_celt_2_5ms() {
        // config=16 → CELT NB 2.5 ms = 120 samples
        assert_eq!(opus_frame_size_samples(16), Some(120));
        // config=20 → CELT WB 2.5 ms = 120 samples
        assert_eq!(opus_frame_size_samples(20), Some(120));
    }

    #[test]
    fn duration_single_frame_celt_fb_20ms() {
        // TOC: config=31 (CELT FB 20ms), s=0, c=0 → 1 frame of 960 samples
        // (matches LiveKit TrackEgress Opus output)
        let toc = (31u8 << 3) | 0b000;
        let packet = vec![toc, 0xAA, 0xBB];
        assert_eq!(opus_packet_duration_samples(&packet), Some(960));
    }

    #[test]
    fn duration_two_frames_celt_fb_20ms() {
        // config=31, c=1 → 2 frames of 960 samples = 1920
        let toc = (31u8 << 3) | 0b001;
        let packet = vec![toc, 0xAA];
        assert_eq!(opus_packet_duration_samples(&packet), Some(1920));
    }
}
