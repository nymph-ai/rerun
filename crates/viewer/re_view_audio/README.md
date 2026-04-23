# re_view_audio

Dedicated view for [`AudioStream`](../../store/re_sdk_types/definitions/rerun/archetypes/audio_stream.fbs) and [`AssetAudio`](../../store/re_sdk_types/definitions/rerun/archetypes/asset_audio.fbs) archetypes.

The view displays per-entity transport state, static codec metadata
(codec, sample rate, channel count), and diagnostic signals (gaps,
discontinuities, out-of-order segments). Playback follows the viewer's
timeline — the view does not own its own clock.

Actual decoding and PCM ring-buffer management live in
[`re_audio`](../../utils/re_audio); this crate is thin glue between the
store-view query path, the `AudioStreamCache`, and egui.
