//! Tracks the decoder's "next sample to emit" position in media time.

use crate::PtsNs;

/// Counts frames already produced by the decoder and converts to nanoseconds.
#[derive(Debug, Clone, Copy)]
pub struct MediaClock {
    sample_rate: u32,
    /// PTS (ns) of the next sample the decoder will emit.
    pts_ns: PtsNs,
}

impl MediaClock {
    /// Construct a clock for a stream at the given sample rate.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            pts_ns: 0,
        }
    }

    /// Current media-time cursor, in ns.
    pub fn pts_ns(&self) -> PtsNs {
        self.pts_ns
    }

    /// After a seek, force the cursor to `target_pts_ns`. The viewer's
    /// `playhead_ns` is accepted only as a sanity argument (logged in
    /// debug builds) so the caller can keep context.
    pub fn realign(&mut self, target_pts_ns: PtsNs, _playhead_ns: PtsNs) {
        self.pts_ns = target_pts_ns;
    }

    /// Advance the clock by `n` emitted frames.
    pub fn advance_frames(&mut self, n: u64) {
        if self.sample_rate == 0 {
            return;
        }
        let delta_ns = (n as i128 * 1_000_000_000 / self.sample_rate as i128) as i64;
        self.pts_ns = self.pts_ns.saturating_add(delta_ns);
    }
}
