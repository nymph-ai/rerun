//! Smoke tests for the audio view.
//!
//! Keeps the surface small — registering the view and logging a couple of
//! audio rows should not panic, and the resulting store should expose the
//! logged components verbatim.

use re_chunk_store::{LatestAtQuery, RowId};
use re_log_types::{TimeInt, TimePoint};
use re_sdk_types::archetypes::{
    AudioAnnotationSpan, AudioEvent, AudioSeekIndex, AudioStream, AudioWaveformSummary,
};
use re_sdk_types::components;
use re_test_context::TestContext;
use re_view_audio::AudioView;

#[test]
fn audio_view_class_registers_and_logs_round_trip() {
    let mut ctx = TestContext::new();
    ctx.register_view_class::<AudioView>();

    let timeline = ctx
        .active_timeline()
        .expect("test context must have an active timeline");

    let static_config = AudioStream::update_fields()
        .with_codec(components::AudioCodec::Opus)
        .with_sample_rate(components::AudioSampleRate(24_000.into()))
        .with_channel_count(components::AudioChannelCount(1.into()))
        .with_source_id("mic");

    let static_point = TimePoint::default();
    ctx.log_entity("/audio", |builder| {
        builder.with_archetype(RowId::new(), static_point.clone(), &static_config)
    });

    // Two synthetic "chunks" — the payload bytes are not real Opus packets,
    // but the visualizer only inspects counts / duration / sequence, not the
    // codec bytes themselves, so this is enough to validate the logging path.
    for (i, pts) in [1_000_000_i64, 21_000_000_i64].into_iter().enumerate() {
        let row = AudioStream::update_fields()
            .with_chunk(components::AudioChunk(vec![0u8; 32].into()))
            .with_duration_samples(components::AudioDurationSamples(480.into()))
            .with_sequence_number(components::AudioSequenceNumber((i as u64).into()))
            .with_discontinuity(components::AudioDiscontinuity(false.into()))
            .with_seekable(components::AudioSeekable(true.into()))
            .with_source_id("mic");

        let timepoint = TimePoint::from([(timeline, pts)]);
        ctx.log_entity("/audio", |builder| {
            builder.with_archetype(RowId::new(), timepoint, &row)
        });
    }

    let waveform = AudioWaveformSummary::new(
        [0_i64, 20_000_000_i64],
        [480_u64, 480_u64],
        [-0.25_f64, -0.10_f64],
        [0.25_f64, 0.10_f64],
    )
    .with_source_id("mic")
    .with_audio_reference("/audio");
    ctx.log_entity("/audio/waveform", |builder| {
        builder.with_archetype(
            RowId::new(),
            TimePoint::from([(timeline, 1_000_000_i64)]),
            &waveform,
        )
    });

    let seek_index = AudioSeekIndex::new(
        [0_i64, 20_000_000_i64],
        [0_i64, 20_000_000_i64],
        [0_u64, 0_u64],
    )
    .with_sequence_number([0_u64, 1_u64])
    .with_discontinuity([false, false])
    .with_source_id("mic")
    .with_audio_reference("/audio");
    ctx.log_entity("/audio/seek", |builder| {
        builder.with_archetype(
            RowId::new(),
            TimePoint::from([(timeline, 1_000_000_i64)]),
            &seek_index,
        )
    });

    let word = AudioAnnotationSpan::new([1_000_000_i64], [19_000_000_i64])
        .with_kind([components::AudioAnnotationKind::Word])
        .with_labels(["hello"])
        .with_confidence([0.98_f64])
        .with_speaker(["speaker-0"])
        .with_source_id("mic")
        .with_audio_reference("/audio");
    ctx.log_entity("/audio/words", |builder| {
        builder.with_archetype(
            RowId::new(),
            TimePoint::from([(timeline, 1_000_000_i64)]),
            &word,
        )
    });

    let event = AudioEvent::new(
        [21_000_000_i64],
        [components::AudioEventKind::EndpointCommit],
    )
    .with_labels(["endpoint committed"])
    .with_source_id("mic")
    .with_audio_reference("/audio");
    ctx.log_entity("/audio/events", |builder| {
        builder.with_archetype(
            RowId::new(),
            TimePoint::from([(timeline, 21_000_000_i64)]),
            &event,
        )
    });

    // Give the chunk store a chance to index the newly logged chunks before
    // we read them back.
    let egui_ctx = egui::Context::default();
    ctx.run_recording(&egui_ctx, |store_ctx| {
        let store = store_ctx.db.storage_engine();
        let latest = store.cache().latest_at(
            &LatestAtQuery::new(*timeline.name(), TimeInt::MAX),
            &"/audio".into(),
            [AudioStream::descriptor_sample_rate().component],
        );
        assert!(
            latest
                .get(AudioStream::descriptor_sample_rate().component)
                .is_some(),
            "sample_rate should be readable back from the store",
        );

        let waveform_latest = store.cache().latest_at(
            &LatestAtQuery::new(*timeline.name(), TimeInt::MAX),
            &"/audio/waveform".into(),
            [AudioWaveformSummary::descriptor_bucket_start().component],
        );
        assert!(
            waveform_latest
                .get(AudioWaveformSummary::descriptor_bucket_start().component)
                .is_some(),
            "waveform buckets should be readable back from the store",
        );

        let seek_latest = store.cache().latest_at(
            &LatestAtQuery::new(*timeline.name(), TimeInt::MAX),
            &"/audio/seek".into(),
            [AudioSeekIndex::descriptor_media_time().component],
        );
        assert!(
            seek_latest
                .get(AudioSeekIndex::descriptor_media_time().component)
                .is_some(),
            "seek entries should be readable back from the store",
        );

        let span_latest = store.cache().latest_at(
            &LatestAtQuery::new(*timeline.name(), TimeInt::MAX),
            &"/audio/words".into(),
            [AudioAnnotationSpan::descriptor_start_time().component],
        );
        assert!(
            span_latest
                .get(AudioAnnotationSpan::descriptor_start_time().component)
                .is_some(),
            "annotation spans should be readable back from the store",
        );

        let event_latest = store.cache().latest_at(
            &LatestAtQuery::new(*timeline.name(), TimeInt::MAX),
            &"/audio/events".into(),
            [AudioEvent::descriptor_event_time().component],
        );
        assert!(
            event_latest
                .get(AudioEvent::descriptor_event_time().component)
                .is_some(),
            "audio events should be readable back from the store",
        );
    });
}
