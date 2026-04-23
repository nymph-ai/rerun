# re_audio

Rerun's audio decoding and playback runtime — the media transport layer for
[`rerun.archetypes.AudioStream`](../../store/re_sdk_types/definitions/rerun/archetypes/audio_stream.fbs)
and [`rerun.archetypes.AssetAudio`](../../store/re_sdk_types/definitions/rerun/archetypes/asset_audio.fbs).

Analog of [`re_video`](../re_video) for audio.

## What lives here

* `decode/` — codec-specific decoders. Native path uses libopus via the
  `opus` crate; wasm path uses WebCodecs `AudioDecoder`.
* `player/` — the stream player: a timeline-driven scheduler that turns a
  sparse stream of encoded segments into an output-rate PCM ring buffer.
* `resampler.rs` — FFT resampler (pure Rust, works on native + wasm) used
  to bridge 24 kHz / 48 kHz / device rates.

## What does *not* live here

* Output sink drivers (`cpal` on native, `AudioWorklet` on web). Those are
  viewer-side glue; this crate exposes the data the sink needs.
* The `AudioStream` / `AssetAudio` Arrow types. `re_audio` is intentionally
  free of `re_sdk_types` so it can be reused by tools that don't depend on
  Rerun's component model.

## System dependencies

Native Opus decoding requires `libopus` on the host:

* Ubuntu / Debian: `apt install libopus-dev`
* macOS: `brew install opus`
* Windows: see [opus-tools](https://opus-codec.org/downloads/) or use
  `vcpkg install opus`.
