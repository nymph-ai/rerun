//! Fixed-size PCM ring buffer drained by the output sink.

/// Interleaved PCM ring buffer.
pub struct SampleRing {
    buf: Vec<f32>,
    capacity_frames: usize,
    channels: usize,
    write_frame: usize,
    read_frame: usize,
    filled_frames: usize,
}

impl SampleRing {
    /// Construct a ring sized for `capacity_frames` interleaved frames.
    pub fn new(capacity_frames: usize, channels: usize) -> Self {
        let capacity_frames = capacity_frames.max(1);
        let channels = channels.max(1);
        Self {
            buf: vec![0.0; capacity_frames * channels],
            capacity_frames,
            channels,
            write_frame: 0,
            read_frame: 0,
            filled_frames: 0,
        }
    }

    /// Number of frames currently buffered.
    pub fn available_read(&self) -> usize {
        self.filled_frames
    }

    /// Number of frames of free space.
    pub fn available_write(&self) -> usize {
        self.capacity_frames - self.filled_frames
    }

    /// Write as many frames as fit.
    pub fn write(&mut self, interleaved: &[f32]) -> usize {
        let frames = interleaved.len() / self.channels;
        let writable = frames.min(self.available_write());
        for frame in 0..writable {
            let src = frame * self.channels;
            let dst = self.write_frame * self.channels;
            self.buf[dst..dst + self.channels]
                .copy_from_slice(&interleaved[src..src + self.channels]);
            self.write_frame = (self.write_frame + 1) % self.capacity_frames;
        }
        self.filled_frames += writable;
        writable
    }

    /// Drain up to `out.len() / channels` frames into `out`. Missing samples
    /// are filled with silence.
    pub fn drain_into(&mut self, out: &mut [f32]) -> usize {
        let want_frames = out.len() / self.channels;
        let take_frames = want_frames.min(self.filled_frames);
        for frame in 0..take_frames {
            let src = self.read_frame * self.channels;
            let dst = frame * self.channels;
            out[dst..dst + self.channels].copy_from_slice(&self.buf[src..src + self.channels]);
            self.read_frame = (self.read_frame + 1) % self.capacity_frames;
        }
        self.filled_frames -= take_frames;
        // Pad remainder with silence.
        if take_frames < want_frames {
            for s in &mut out[take_frames * self.channels..] {
                *s = 0.0;
            }
        }
        take_frames
    }

    /// Briefly ramp the head of the ring to zero, to avoid a click when
    /// transitioning out of playback. Cheap implementation: zero the ring.
    pub fn mute_fade(&mut self) {
        self.flush();
    }

    /// Drop all buffered samples.
    pub fn flush(&mut self) {
        self.write_frame = 0;
        self.read_frame = 0;
        self.filled_frames = 0;
        for s in &mut self.buf {
            *s = 0.0;
        }
    }
}
