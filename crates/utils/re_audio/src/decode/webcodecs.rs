//! WebCodecs `AudioDecoder` bridge for wasm targets.
//!
//! WebCodecs is an async callback-based API. `AudioStreamPlayer` on the web
//! drives it by queuing encoded chunks and collecting decoded PCM from the
//! `output` callback; this module exposes the thin synchronous shim the
//! player expects.

use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{AudioData, AudioDecoderConfig, AudioDecoderInit, EncodedAudioChunkInit};
// (Re-exported here for feature-gating clarity; referenced inside closures.)

use crate::{
    DecodeError, PtsNs,
    codec::AudioCodecKind,
    decode::{AudioDecoder, DecodedAudio},
};

/// Decoder backed by the browser's `AudioDecoder`.
pub struct WebCodecsDecoder {
    inner: web_sys::AudioDecoder,
    pending: std::rc::Rc<std::cell::RefCell<Vec<DecodedAudio>>>,
    codec: AudioCodecKind,
    sample_rate: u32,
    channels: u16,
    _on_output: Closure<dyn FnMut(JsValue)>,
    _on_error: Closure<dyn FnMut(JsValue)>,
}

impl WebCodecsDecoder {
    /// Create a WebCodecs decoder for the given codec / output format.
    pub fn new(codec: AudioCodecKind, sample_rate: u32, channels: u16) -> Result<Self, JsValue> {
        let pending = std::rc::Rc::new(std::cell::RefCell::new(Vec::<DecodedAudio>::new()));

        let pending_out = pending.clone();
        let on_output = Closure::wrap(Box::new(move |value: JsValue| {
            if let Ok(audio_data) = value.dyn_into::<AudioData>() {
                let frames = audio_data.number_of_frames() as usize;
                let channels = audio_data.number_of_channels() as u16;
                let sample_rate = audio_data.sample_rate() as u32;
                let pts_ns = (audio_data.timestamp() as i64) * 1000; // µs → ns

                let sample_count = frames * channels as usize;
                // WebCodecs delivers f32 PCM as little-endian bytes. We copy
                // into a byte buffer then reinterpret as f32.
                let mut bytes = vec![0u8; sample_count * std::mem::size_of::<f32>()];
                let copy_opts = web_sys::AudioDataCopyToOptions::new(0);
                let _ = audio_data.copy_to_with_u8_slice(&mut bytes, &copy_opts);
                let mut samples = Vec::<f32>::with_capacity(sample_count);
                for chunk in bytes.chunks_exact(std::mem::size_of::<f32>()) {
                    samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
                pending_out.borrow_mut().push(DecodedAudio {
                    samples,
                    channels,
                    sample_rate,
                    pts_ns,
                });
                audio_data.close();
            }
        }) as Box<dyn FnMut(JsValue)>);

        let on_error = Closure::wrap(Box::new(|err: JsValue| {
            re_log::warn!("webcodecs decoder error: {err:?}");
        }) as Box<dyn FnMut(JsValue)>);

        let init = AudioDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_output.as_ref().unchecked_ref(),
        );
        let inner = web_sys::AudioDecoder::new(&init)?;

        let config =
            AudioDecoderConfig::new(codec.webcodecs_string(), sample_rate, channels as u32);
        inner.configure(&config)?;

        Ok(Self {
            inner,
            pending,
            codec,
            sample_rate,
            channels,
            _on_output: on_output,
            _on_error: on_error,
        })
    }
}

impl AudioDecoder for WebCodecsDecoder {
    fn decode(&mut self, chunk: &[u8], pts_ns: PtsNs) -> Result<DecodedAudio, DecodeError> {
        // Convert ns → µs for WebCodecs.
        let pts_us = pts_ns / 1000;
        let buffer = js_sys::Uint8Array::new_with_length(chunk.len() as u32);
        buffer.copy_from(chunk);
        // WebCodecs' timestamp field is currently typed `i32` in web-sys;
        // clamp to prevent wrap-around on very long streams.
        let pts_arg = pts_us.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let init = EncodedAudioChunkInit::new(
            &buffer.into(),
            pts_arg,
            web_sys::EncodedAudioChunkType::Key,
        );
        let encoded = web_sys::EncodedAudioChunk::new(&init)
            .map_err(|e| DecodeError::Backend(format!("EncodedAudioChunk: {e:?}")))?;
        self.inner
            .decode(&encoded)
            .map_err(|e| DecodeError::Backend(format!("AudioDecoder.decode: {e:?}")))?;

        // WebCodecs decodes asynchronously. The player polls `pending` each
        // frame; here we return the first available or an empty frame.
        if let Some(d) = self.pending.borrow_mut().pop() {
            Ok(d)
        } else {
            Ok(DecodedAudio {
                samples: Vec::new(),
                channels: self.channels,
                sample_rate: self.sample_rate,
                pts_ns,
            })
        }
    }

    fn reset(&mut self) {
        let _ = self.inner.reset();
        self.pending.borrow_mut().clear();
    }

    fn codec(&self) -> AudioCodecKind {
        self.codec
    }
}

// SAFETY: wasm32 has no real OS threads — every handle we hold (the
// JS-wrapped decoder, pending vec, and closures) is confined to the main
// browser thread.
#[expect(unsafe_code)]
#[expect(clippy::undocumented_unsafe_blocks)]
unsafe impl Send for WebCodecsDecoder {}
#[expect(unsafe_code)]
#[expect(clippy::undocumented_unsafe_blocks)]
unsafe impl Sync for WebCodecsDecoder {}
