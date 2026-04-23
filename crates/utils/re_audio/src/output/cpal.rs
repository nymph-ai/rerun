//! `cpal`-backed output sink.
//!
//! Opens the system default output device and pumps samples from an internal
//! bounded ring buffer to the audio callback. The ring buffer is guarded by a
//! parking-lot mutex — not wait-free, but contention is light because the
//! audio thread only holds the lock for the duration of a single copy.
//!
//! Because `cpal::Stream` is neither `Send` nor `Sync` on most platforms, the
//! stream is parked on a dedicated thread (see [`SpawnedCpalSink`]). Callers
//! that need a `Send + Sync` handle push samples through the handle; the
//! audio callback reads from the same shared ring.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use cpal::{
    Stream,
    traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _},
};
use parking_lot::Mutex;

use super::AudioSink;

/// Errors surfaced while building a [`CpalSink`].
#[derive(Debug, thiserror::Error)]
pub enum CpalSinkError {
    /// No default output device was available.
    #[error("no default output audio device")]
    NoDefaultDevice,

    /// The device could not report a supported output stream configuration.
    #[error("no supported output config: {0}")]
    NoSupportedConfig(#[source] cpal::DefaultStreamConfigError),

    /// Building the audio stream failed.
    #[error("failed to build audio stream: {0}")]
    BuildStream(#[from] cpal::BuildStreamError),

    /// Starting the audio stream failed.
    #[error("failed to start audio stream: {0}")]
    PlayStream(#[from] cpal::PlayStreamError),
}

/// Fixed-size interleaved ring buffer.
struct Ring {
    buf: Vec<f32>,
    write: usize,
    read: usize,
    filled: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0.0; capacity.max(1)],
            write: 0,
            read: 0,
            filled: 0,
        }
    }

    fn push(&mut self, data: &[f32]) -> usize {
        let free = self.buf.len() - self.filled;
        let take = data.len().min(free);
        for &sample in &data[..take] {
            self.buf[self.write] = sample;
            self.write = (self.write + 1) % self.buf.len();
        }
        self.filled += take;
        take
    }

    fn pop_into(&mut self, out: &mut [f32]) -> usize {
        let take = out.len().min(self.filled);
        for slot in out.iter_mut().take(take) {
            *slot = self.buf[self.read];
            self.read = (self.read + 1) % self.buf.len();
        }
        self.filled -= take;
        take
    }

    fn clear(&mut self) {
        self.write = 0;
        self.read = 0;
        self.filled = 0;
    }
}

/// Active `cpal` output sink.
///
/// Dropping the sink stops the underlying stream.
pub struct CpalSink {
    sample_rate: u32,
    channels: u16,
    ring: Arc<Mutex<Ring>>,
    _stream: Stream,
}

impl CpalSink {
    /// Open the default output device.
    ///
    /// `desired_rate` and `desired_channels` are hints — if the device does
    /// not support them exactly, the host's default config is used instead,
    /// and the actual values are reflected on the returned sink.
    pub fn open(
        desired_rate: Option<u32>,
        desired_channels: Option<u16>,
        ring_frames: usize,
    ) -> Result<Self, CpalSinkError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(CpalSinkError::NoDefaultDevice)?;
        let supported = device
            .default_output_config()
            .map_err(CpalSinkError::NoSupportedConfig)?;

        let sample_rate = desired_rate.unwrap_or_else(|| supported.sample_rate().0);
        let channels = desired_channels.unwrap_or_else(|| supported.channels());

        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let ring = Arc::new(Mutex::new(Ring::new(ring_frames * channels as usize)));

        let ring_for_cb = ring.clone();
        let err_fn = |e| re_log::error!("cpal stream error: {e}");
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
                &config,
                move |out, _| {
                    let mut ring = ring_for_cb.lock();
                    let filled = ring.pop_into(out);
                    // Zero-fill the remainder so the output doesn't repeat stale
                    // samples when the player underruns.
                    for slot in &mut out[filled..] {
                        *slot = 0.0;
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
                &config,
                move |out, _| {
                    let mut tmp = vec![0.0_f32; out.len()];
                    let filled = ring_for_cb.lock().pop_into(&mut tmp);
                    for (i, slot) in out.iter_mut().enumerate() {
                        let s = if i < filled { tmp[i] } else { 0.0 };
                        *slot = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err_fn,
                None,
            )?,
            cpal::SampleFormat::U16 => device.build_output_stream::<u16, _, _>(
                &config,
                move |out, _| {
                    let mut tmp = vec![0.0_f32; out.len()];
                    let filled = ring_for_cb.lock().pop_into(&mut tmp);
                    for (i, slot) in out.iter_mut().enumerate() {
                        let s = if i < filled { tmp[i] } else { 0.0 };
                        let scaled = ((s.clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16;
                        *slot = scaled;
                    }
                },
                err_fn,
                None,
            )?,
            other => {
                re_log::warn!("unsupported cpal sample format {other:?}, falling back to f32");
                device.build_output_stream::<f32, _, _>(
                    &config,
                    move |out, _| {
                        let mut ring = ring_for_cb.lock();
                        let filled = ring.pop_into(out);
                        for slot in &mut out[filled..] {
                            *slot = 0.0;
                        }
                    },
                    err_fn,
                    None,
                )?
            }
        };

        stream.play()?;

        Ok(Self {
            sample_rate,
            channels,
            ring,
            _stream: stream,
        })
    }
}

impl AudioSink for CpalSink {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn push(&self, interleaved: &[f32]) -> usize {
        self.ring.lock().push(interleaved)
    }

    fn flush(&self) {
        self.ring.lock().clear();
    }
}

/// `Send + Sync` handle backed by a dedicated audio thread.
///
/// Construct one with [`SpawnedCpalSink::open`]. The thread lives as long as
/// the handle. Dropping the handle drops the `cpal::Stream` which stops
/// playback.
pub struct SpawnedCpalSink {
    sample_rate: u32,
    channels: u16,
    ring: Arc<Mutex<Ring>>,
    shutdown_tx: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SpawnedCpalSink {
    /// Open the default output device on a dedicated thread.
    pub fn open(
        desired_rate: Option<u32>,
        desired_channels: Option<u16>,
        ring_frames: usize,
    ) -> Result<Self, CpalSinkError> {
        #[expect(clippy::disallowed_methods)]
        let (open_tx, open_rx) = mpsc::channel::<Result<OpenReply, CpalSinkError>>();
        #[expect(clippy::disallowed_methods)]
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

        let thread = thread::Builder::new()
            .name("re_audio::cpal-sink".into())
            .spawn(
                move || match CpalSink::open(desired_rate, desired_channels, ring_frames) {
                    Ok(sink) => {
                        let reply = OpenReply {
                            sample_rate: sink.sample_rate,
                            channels: sink.channels,
                            ring: sink.ring.clone(),
                        };
                        if open_tx.send(Ok(reply)).is_err() {
                            return; // caller gave up before we finished opening
                        }
                        // Keep the CpalSink (and therefore the Stream) alive
                        // until the handle is dropped. Either side closing
                        // the shutdown channel wakes us up.
                        let _sink = sink;
                        let _received: Result<(), _> = shutdown_rx.recv();
                    }
                    Err(err) => {
                        let _sent: Result<(), _> = open_tx.send(Err(err));
                    }
                },
            )
            .map_err(|io_err| {
                CpalSinkError::BuildStream(cpal::BuildStreamError::BackendSpecific {
                    err: cpal::BackendSpecificError {
                        description: format!("failed to spawn cpal thread: {io_err}"),
                    },
                })
            })?;

        let reply = open_rx.recv().map_err(|_err| {
            CpalSinkError::BuildStream(cpal::BuildStreamError::BackendSpecific {
                err: cpal::BackendSpecificError {
                    description: "cpal thread exited before reporting open status".to_owned(),
                },
            })
        })??;

        Ok(Self {
            sample_rate: reply.sample_rate,
            channels: reply.channels,
            ring: reply.ring,
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        })
    }
}

impl Drop for SpawnedCpalSink {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            drop(tx);
        }
        if let Some(thread) = self.thread.take() {
            let _joined: std::thread::Result<()> = thread.join();
        }
    }
}

impl AudioSink for SpawnedCpalSink {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn push(&self, interleaved: &[f32]) -> usize {
        self.ring.lock().push(interleaved)
    }

    fn flush(&self) {
        self.ring.lock().clear();
    }
}

struct OpenReply {
    sample_rate: u32,
    channels: u16,
    ring: Arc<Mutex<Ring>>,
}
