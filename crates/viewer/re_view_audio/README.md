# re_view_audio

Dedicated view for [`AudioStream`](../../store/re_sdk_types/definitions/rerun/archetypes/audio_stream.fbs),
[`AssetAudio`](../../store/re_sdk_types/definitions/rerun/archetypes/asset_audio.fbs),
and audio-aligned summary/annotation/event archetypes.

The view displays per-entity transport state, static codec metadata
(codec, sample rate, channel count), and diagnostic signals (gaps,
discontinuities, out-of-order segments). Playback follows the viewer's
timeline — the view does not own its own clock.

Waveform summaries, seek indexes, annotation spans, and point events are
rendered as timeline lanes over queryable logged data. The implementation
architecture is documented in [ARCHITECTURE.md](ARCHITECTURE.md).

Actual decoding and PCM ring-buffer management live in
[`re_audio`](../../utils/re_audio); this crate is thin glue between the
store-view query path, the `AudioStreamCache`, and egui.
