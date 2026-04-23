use std::collections::BTreeMap;

use re_chunk_store::{AbsoluteTimeRange, LatestAtQuery, RangeQuery};
use re_log_types::{EntityPath, TimeInt};
use re_sdk_types::Archetype as _;
use re_sdk_types::archetypes::AudioStream;
use re_sdk_types::components;
use re_viewer_context::{
    AudioStreamCache, IdentifiedViewSystem, ViewContext, ViewContextCollection, ViewQuery,
    ViewSystemExecutionError, VisualizerExecutionOutput, VisualizerQueryInfo, VisualizerSystem,
};

/// Per-entity summary produced by the audio visualizer and consumed by the view UI.
#[derive(Clone, Debug)]
pub struct AudioStreamSummary {
    pub entity_path: EntityPath,

    /// Static configuration, or `None` if the required components have not
    /// been logged yet on this entity.
    pub config: Option<AudioStreamConfig>,

    /// Number of chunk rows observed in the queried range.
    pub segment_count: usize,

    /// Sum of `duration_samples` across observed rows. `None` if any row is
    /// missing its duration column.
    pub total_samples: Option<u64>,

    /// Number of rows whose `pts_ns` was strictly less than the previous
    /// row's `pts_ns`. Any non-zero value indicates the viewer will rely on
    /// nearest-prior-seekable logic for scrubbing.
    pub out_of_order: usize,

    /// Number of rows whose `discontinuity` flag is true.
    pub discontinuities: usize,

    /// Number of gaps observed in `sequence_number` (missing or non-unit
    /// increments). `None` if sequence numbers are not logged.
    pub sequence_gaps: Option<usize>,

    /// Presentation timestamp of the first and last observed row.
    pub pts_range: Option<(TimeInt, TimeInt)>,
}

/// Snapshot of the static configuration for an [`AudioStream`].
#[derive(Clone, Copy, Debug)]
pub struct AudioStreamConfig {
    pub codec: components::AudioCodec,
    pub sample_rate: u32,
    pub channel_count: u16,
}

/// The audio visualizer — produces summary entries for the audio view.
#[derive(Default)]
pub struct AudioStreamVisualizerSystem;

impl IdentifiedViewSystem for AudioStreamVisualizerSystem {
    fn identifier() -> re_viewer_context::ViewSystemIdentifier {
        "AudioStream".into()
    }
}

impl VisualizerSystem for AudioStreamVisualizerSystem {
    fn visualizer_query_info(
        &self,
        _app_options: &re_viewer_context::AppOptions,
    ) -> VisualizerQueryInfo {
        VisualizerQueryInfo::single_required_component::<components::AudioChunk>(
            &AudioStream::descriptor_chunk(),
            &AudioStream::all_components(),
        )
    }

    fn execute(
        &self,
        ctx: &ViewContext<'_>,
        view_query: &ViewQuery<'_>,
        _context_systems: &ViewContextCollection,
    ) -> Result<VisualizerExecutionOutput, ViewSystemExecutionError> {
        re_tracing::profile_function!();

        let output = VisualizerExecutionOutput::default();
        let mut summaries: BTreeMap<EntityPath, AudioStreamSummary> = BTreeMap::new();

        for (data_result, _instruction) in
            view_query.iter_visualizer_instruction_for(Self::identifier())
        {
            let summary = build_summary(ctx, data_result.entity_path.clone(), view_query);
            summaries.insert(data_result.entity_path.clone(), summary);
        }

        Ok(output.with_visualizer_data(summaries))
    }
}

fn build_summary(
    ctx: &ViewContext<'_>,
    entity_path: EntityPath,
    view_query: &ViewQuery<'_>,
) -> AudioStreamSummary {
    let store = ctx.recording();

    let config = resolve_static_config(ctx, &entity_path, view_query);

    // Keep the stream player cached across frames so the eventual audio sink
    // (cpal / WebAudio) can consume PCM without re-constructing the decoder
    // every repaint. Errors are not fatal here — the UI still renders the
    // diagnostic summary when the player can't be constructed.
    if let Some(cfg) = &config {
        let timeline = view_query.timeline;
        let entity_path_for_cache = entity_path.clone();
        let source_rate = cfg.sample_rate;
        ctx.viewer_ctx
            .store_context
            .memoizer(|cache: &mut AudioStreamCache| {
                if let Ok(stream) = cache.audio_entry(
                    store,
                    &entity_path_for_cache,
                    timeline,
                    source_rate,
                    source_rate.max(1) as usize,
                ) {
                    AudioStreamCache::refresh_segments_from_store(
                        &stream,
                        store,
                        &entity_path_for_cache,
                        timeline,
                        AbsoluteTimeRange::EVERYTHING,
                    );
                }
            });
    }

    let chunk_descr = AudioStream::descriptor_chunk();
    let duration_descr = AudioStream::descriptor_duration_samples();
    let seekable_descr = AudioStream::descriptor_seekable();
    let discontinuity_descr = AudioStream::descriptor_discontinuity();
    let sequence_descr = AudioStream::descriptor_sequence_number();

    let timeline = view_query.timeline;
    let query = RangeQuery::new(timeline, AbsoluteTimeRange::EVERYTHING);
    let chunks = store.storage_engine().store().range_relevant_chunks(
        re_chunk_store::ChunkTrackingMode::Ignore,
        &query,
        &entity_path,
        chunk_descr.component,
    );

    let mut segment_count = 0;
    let mut total_samples_u: u64 = 0;
    let mut any_duration_missing = false;
    let mut discontinuities = 0;
    let mut out_of_order = 0;
    let mut any_sequence_seen = false;
    let mut sequence_gaps = 0;
    let mut last_sequence: Option<u64> = None;
    let mut last_pts: Option<TimeInt> = None;
    let mut first_pts: Option<TimeInt> = None;

    for chunk in &chunks.chunks {
        let index_iter = chunk.iter_component_indices(timeline, chunk_descr.component);
        let duration_iter =
            chunk.iter_component::<components::AudioDurationSamples>(duration_descr.component);
        let _seekable_iter =
            chunk.iter_component::<components::AudioSeekable>(seekable_descr.component);
        let discontinuity_iter =
            chunk.iter_component::<components::AudioDiscontinuity>(discontinuity_descr.component);
        let sequence_iter =
            chunk.iter_component::<components::AudioSequenceNumber>(sequence_descr.component);

        for ((((time, _row_id), duration_item), discontinuity_item), sequence_item) in index_iter
            .zip(duration_iter)
            .zip(discontinuity_iter)
            .zip(sequence_iter)
        {
            segment_count += 1;

            if let Some(duration) = duration_item.as_slice().first() {
                total_samples_u = total_samples_u.saturating_add(duration.0.0);
            } else {
                any_duration_missing = true;
            }

            if discontinuity_item.as_slice().first().is_some_and(|d| d.0.0) {
                discontinuities += 1;
            }

            if let Some(seq) = sequence_item.as_slice().first() {
                any_sequence_seen = true;
                if let Some(prev) = last_sequence
                    && seq.0.0 != prev.saturating_add(1)
                {
                    sequence_gaps += 1;
                }
                last_sequence = Some(seq.0.0);
            }

            if first_pts.is_none() {
                first_pts = Some(time);
            }
            if let Some(prev) = last_pts
                && time < prev
            {
                out_of_order += 1;
            }
            last_pts = Some(time);
        }
    }

    AudioStreamSummary {
        entity_path,
        config,
        segment_count,
        total_samples: if any_duration_missing {
            None
        } else {
            Some(total_samples_u)
        },
        out_of_order,
        discontinuities,
        sequence_gaps: if any_sequence_seen {
            Some(sequence_gaps)
        } else {
            None
        },
        pts_range: match (first_pts, last_pts) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        },
    }
}

fn resolve_static_config(
    ctx: &ViewContext<'_>,
    entity_path: &EntityPath,
    view_query: &ViewQuery<'_>,
) -> Option<AudioStreamConfig> {
    let store = ctx.recording();
    let timeline = view_query.timeline;

    let codec_descr = AudioStream::descriptor_codec();
    let rate_descr = AudioStream::descriptor_sample_rate();
    let channels_descr = AudioStream::descriptor_channel_count();

    let latest = store.storage_engine().cache().latest_at(
        &LatestAtQuery::new(timeline, TimeInt::MAX),
        entity_path,
        [
            codec_descr.component,
            rate_descr.component,
            channels_descr.component,
        ],
    );

    let codec = latest
        .get(codec_descr.component)?
        .component_mono::<components::AudioCodec>(codec_descr.component)?
        .ok()?;
    let sample_rate = latest
        .get(rate_descr.component)?
        .component_mono::<components::AudioSampleRate>(rate_descr.component)?
        .ok()?;
    let channel_count = latest
        .get(channels_descr.component)?
        .component_mono::<components::AudioChannelCount>(channels_descr.component)?
        .ok()?;

    Some(AudioStreamConfig {
        codec,
        sample_rate: sample_rate.0.0,
        channel_count: channel_count.0.0,
    })
}
