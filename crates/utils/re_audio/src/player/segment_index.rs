//! Sorted index of encoded segments by presentation timestamp.

use std::collections::BTreeMap;

use crate::PtsNs;

/// Metadata for a single encoded segment known to the player.
#[derive(Debug, Clone)]
pub struct SegmentRef {
    /// Presentation timestamp of the first sample, in nanoseconds.
    pub pts_ns: PtsNs,
    /// Duration of the segment, in nanoseconds.
    pub duration_ns: i64,
    /// Encoded payload (ref-counted in the viewer; `re_audio` borrows).
    pub chunk: Vec<u8>,
    /// Whether this segment is an independent seek target.
    pub seekable: bool,
    /// Whether the decoder must reset before this segment.
    pub discontinuity: bool,
}

/// BTreeMap-backed index of segments keyed by `pts_ns`.
#[derive(Default)]
pub struct SegmentIndex {
    inner: BTreeMap<PtsNs, SegmentRef>,
}

impl SegmentIndex {
    /// Empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite a segment.
    pub fn insert(&mut self, seg: SegmentRef) {
        self.inner.insert(seg.pts_ns, seg);
    }

    /// Remove all segments.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Number of segments known.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True if no segments known.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Closest seekable segment at or before `pts_ns`.
    pub fn nearest_prior_seekable(&self, pts_ns: PtsNs) -> Option<&SegmentRef> {
        self.inner
            .range(..=pts_ns)
            .rev()
            .find_map(|(_, seg)| seg.seekable.then_some(seg))
    }

    /// First segment whose PTS is ≥ `pts_ns`.
    pub fn next_at_or_after(&self, pts_ns: PtsNs) -> Option<&SegmentRef> {
        self.inner.range(pts_ns..).next().map(|(_, s)| s)
    }

    /// Iterate segments whose PTS falls inside `[start, end)`.
    pub fn range_ns(&self, start: PtsNs, end: PtsNs) -> impl Iterator<Item = &SegmentRef> {
        self.inner.range(start..end).map(|(_, s)| s)
    }
}
