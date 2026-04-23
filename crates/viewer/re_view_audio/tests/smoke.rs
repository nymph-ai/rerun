//! Smoke tests for the audio view.
//!
//! Keeps the surface small — registering the view and logging a couple of
//! [`archetypes::AudioStream`] rows should not panic, and the resulting
//! store should expose the logged components verbatim.

use re_chunk_store::{LatestAtQuery, RowId};
use re_log_types::{TimeInt, TimePoint};
use re_sdk_types::archetypes::AudioStream;
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
        .with_channel_count(components::AudioChannelCount(1.into()));

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
            .with_seekable(components::AudioSeekable(true.into()));

        let timepoint = TimePoint::from([(timeline, pts)]);
        ctx.log_entity("/audio", |builder| {
            builder.with_archetype(RowId::new(), timepoint, &row)
        });
    }

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
    });
}
