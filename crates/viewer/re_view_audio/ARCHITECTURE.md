# Observability-Grade Audio Replay

This view is the first implementation slice of native audio replay for Rerun.
It is intentionally a replay and inspection surface, not a DAW/editor.

## Layers

The implementation follows five layers:

1. Source data: `AssetAudio` for immutable complete recordings, `AudioStream`
   for incrementally logged decodeable segments.
2. Derived data: `AudioWaveformSummary` and `AudioSeekIndex` are logged
   archetypes, so waveform pyramids and seek lookup tables remain queryable
   and exportable data products rather than private viewer state.
3. Playback runtime: `re_audio` owns decoder construction, stream segment
   indexing, PCM buffering, resampling, transport following, and output sinks.
4. Annotation data: `AudioAnnotationSpan` and `AudioEvent` carry ASR,
   diarization, endpointing, interruption, TTS, and agent/event evidence on the
   same media timeline as source audio.
5. Audio timeline view: `re_view_audio` queries all of the above and presents
   source, derived, annotation, and event lanes with one viewer playhead.

## Source Model

`AudioStream` rows are decodeable time-bounded segments, not low-level transport
packets. Each row can carry codec metadata, presentation time, duration in
samples, sequence number, discontinuity state, seekability, and a stable
`AudioSourceId`.

`AssetAudio` carries complete encoded media plus metadata such as media type,
codec, sample rate, channel count, duration, and `AudioSourceId`.

The public schema is codec-tagged and codec-agnostic. Playback support is
runtime-specific.

## Runtime Contract

`re_audio::DecoderRegistry` maps `AudioCodecKind` to decoder factories. The
view can therefore render source metadata, summaries, seek indexes, annotations,
and events even when a local build cannot decode a particular codec.

The playback path remains:

`viewer transport -> segment resolver -> decoder -> PCM staging -> resampler/mixer -> output sink`

The viewer timeline is the user-facing clock. The audio runtime follows that
clock, handles seek/reset/discontinuity events, and keeps output buffers fed.

## View Lanes

The audio view registers visualizers for:

- `AudioStream`: source diagnostics and playback ownership.
- `AudioWaveformSummary`: derived waveform bucket lanes.
- `AudioSeekIndex`: materialized seek/index diagnostics.
- `AudioAnnotationSpan`: ASR, word, token, speaker, VAD, endpointing, TTS,
  interruption, and review spans.
- `AudioEvent`: decoder, ASR, diarization, barge-in, endpointing, agent-state,
  policy, latency, and discontinuity markers.

This is deliberately data-first. Lane state comes from archetypes/components in
the store; user-specific configuration such as visible lanes, ordering,
pre/post-roll, colors, and follow mode belongs in blueprint/view state.

## Selection Semantics

A selection can resolve to a point, time range, span entity, or linked set of
spans/events. The intended replay path is:

1. Resolve selection to source id, media time range, and optional pre/post-roll.
2. Ask `AudioSeekIndex` for the nearest preceding decode boundary.
3. Decode forward, trim samples to the requested start, and audition the region.
4. Keep all lanes visually aligned to the viewer timeline while playback runs.

The current implementation establishes the durable schema, generated SDK
surface, decoder registry boundary, and view-query lane registration needed for
that deeper selection/audition flow.
