//! Thin wrapper around `rubato` for resampling decoded PCM to the output
//! device rate with a small drift-correction bias.
//!
//! We pick a FFT-based resampler for quality; when the input rate already
//! matches the output rate the resampler short-circuits to an identity pass.

use rubato::{FftFixedInOut, Resampler as _};

/// Minimum number of input frames processed per resampler call.
///
/// Small enough to keep latency low, large enough to keep FFT bins stable.
const CHUNK_FRAMES: usize = 1024;

/// Linear-quality resampler operating on interleaved PCM.
pub struct InterleavedResampler {
    inner: Option<FftFixedInOut<f32>>,
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    in_planar: Vec<Vec<f32>>,
    out_planar: Vec<Vec<f32>>,
}

impl InterleavedResampler {
    /// Construct a resampler that maps `in_rate` → `out_rate`.
    pub fn new(
        in_rate: u32,
        out_rate: u32,
        channels: u16,
    ) -> Result<Self, rubato::ResamplerConstructionError> {
        let channels = channels as usize;
        let inner = if in_rate == out_rate {
            None
        } else {
            Some(FftFixedInOut::<f32>::new(
                in_rate as usize,
                out_rate as usize,
                CHUNK_FRAMES,
                channels,
            )?)
        };

        Ok(Self {
            inner,
            in_rate,
            out_rate,
            channels,
            in_planar: vec![Vec::new(); channels],
            out_planar: vec![Vec::new(); channels],
        })
    }

    /// Process `pcm_interleaved` into resampled interleaved PCM.
    ///
    /// If the input and output rates match, this returns the input slice's
    /// contents directly in the output.
    pub fn process(&mut self, pcm_interleaved: &[f32]) -> Vec<f32> {
        if self.inner.is_none() {
            return pcm_interleaved.to_vec();
        }

        // De-interleave into planar buffers.
        let frames = pcm_interleaved.len() / self.channels;
        for ch in 0..self.channels {
            self.in_planar[ch].clear();
            self.in_planar[ch].reserve(frames);
        }
        for frame in 0..frames {
            for ch in 0..self.channels {
                self.in_planar[ch].push(pcm_interleaved[frame * self.channels + ch]);
            }
        }

        let inner = self.inner.as_mut().expect("rate differs → inner present");

        let expected = inner.input_frames_next();
        if self.in_planar[0].len() < expected {
            // Not enough data this call — caller will feed more on the next
            // tick. Return empty to signal "hold fire".
            return Vec::new();
        }

        for ch in 0..self.channels {
            self.out_planar[ch].resize(inner.output_frames_next(), 0.0);
        }

        let (_in_used, out_frames) = inner
            .process_into_buffer(&self.in_planar, &mut self.out_planar, None)
            .unwrap_or((0, 0));

        // Re-interleave output.
        let mut out = Vec::with_capacity(out_frames * self.channels);
        for frame in 0..out_frames {
            for ch in 0..self.channels {
                out.push(self.out_planar[ch][frame]);
            }
        }
        out
    }

    /// Input sample rate, in Hz.
    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Output sample rate, in Hz.
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }
}
