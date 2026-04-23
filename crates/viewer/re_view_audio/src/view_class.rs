use std::collections::BTreeMap;

use re_log_types::EntityPath;
use re_sdk_types::blueprint::components::PlayState;
use re_sdk_types::{View as _, ViewClassIdentifier, components};
use re_ui::{Help, UiExt as _, icons};
use re_viewer_context::{
    AudioStreamCache, IdentifiedViewSystem as _, IndicatedEntities, PerVisualizerType,
    RecommendedVisualizers, ViewClass, ViewClassRegistryError, ViewId, ViewQuery, ViewState,
    ViewStateExt as _, ViewSystemExecutionError, ViewSystemIdentifier, ViewerContext,
    VisualizableReason, suggest_view_for_each_entity,
};

use crate::playback_state::{AudioViewState, ViewTransport};
use crate::visualizer_system::{
    AudioStreamConfig, AudioStreamSummary, AudioStreamVisualizerSystem,
};

#[derive(Default)]
pub struct AudioView;

type ViewType = re_sdk_types::blueprint::views::AudioView;

impl ViewClass for AudioView {
    fn identifier() -> ViewClassIdentifier {
        ViewType::identifier()
    }

    fn display_name(&self) -> &'static str {
        "Audio"
    }

    fn icon(&self) -> &'static re_ui::Icon {
        // No dedicated audio icon yet; reuse the graph icon as a placeholder.
        &icons::VIEW_GRAPH
    }

    fn new_state(&self) -> Box<dyn ViewState> {
        Box::<AudioViewState>::default()
    }

    fn help(&self, _os: egui::os::OperatingSystem) -> Help {
        Help::new("Audio view").docs_link("https://rerun.io/docs/reference/types/views/audio_view")
    }

    fn on_register(
        &self,
        system_registry: &mut re_viewer_context::ViewSystemRegistrator<'_>,
    ) -> Result<(), ViewClassRegistryError> {
        system_registry.register_visualizer::<AudioStreamVisualizerSystem>()?;
        Ok(())
    }

    fn preferred_tile_aspect_ratio(&self, _state: &dyn ViewState) -> Option<f32> {
        None
    }

    fn recommended_visualizers_for_entity(
        &self,
        _entity_path: &EntityPath,
        visualizers_with_reason: &[(ViewSystemIdentifier, &VisualizableReason)],
        _indicated_entities_per_visualizer: &PerVisualizerType<&IndicatedEntities>,
    ) -> RecommendedVisualizers {
        if visualizers_with_reason
            .iter()
            .any(|(viz, _)| *viz == AudioStreamVisualizerSystem::identifier())
        {
            RecommendedVisualizers::default(AudioStreamVisualizerSystem::identifier())
        } else {
            RecommendedVisualizers::empty()
        }
    }

    fn spawn_heuristics(
        &self,
        ctx: &ViewerContext<'_>,
        include_entity: &dyn Fn(&EntityPath) -> bool,
    ) -> re_viewer_context::ViewSpawnHeuristics {
        re_tracing::profile_function!();
        suggest_view_for_each_entity::<AudioStreamVisualizerSystem>(ctx, include_entity)
    }

    fn layout_priority(&self) -> re_viewer_context::ViewClassLayoutPriority {
        re_viewer_context::ViewClassLayoutPriority::Low
    }

    fn selection_ui(
        &self,
        _ctx: &ViewerContext<'_>,
        ui: &mut egui::Ui,
        state: &mut dyn ViewState,
        _space_origin: &EntityPath,
        _view_id: ViewId,
    ) -> Result<(), ViewSystemExecutionError> {
        let state = state.downcast_mut::<AudioViewState>()?;
        ui.label("The audio view follows the viewer timeline.");
        let mut muted = state.transport == ViewTransport::Muted;
        if ui.re_checkbox(&mut muted, "Mute audio output").changed() {
            state.transport = if muted {
                ViewTransport::Muted
            } else {
                ViewTransport::FollowTimeline
            };
        }
        Ok(())
    }

    fn ui(
        &self,
        ctx: &ViewerContext<'_>,
        _missing_chunk_reporter: &re_viewer_context::MissingChunkReporter,
        ui: &mut egui::Ui,
        state: &mut dyn ViewState,
        query: &ViewQuery<'_>,
        system_output: re_viewer_context::SystemExecutionOutput,
    ) -> Result<(), ViewSystemExecutionError> {
        let state = state.downcast_mut::<AudioViewState>()?;

        let summaries = system_output.visualizer_data::<BTreeMap<EntityPath, AudioStreamSummary>>(
            AudioStreamVisualizerSystem::identifier(),
        )?;

        let live: Vec<EntityPath> = summaries.keys().cloned().collect();
        state.forget_owner_if_missing(&live);

        // Drive the active sink — the first entity with a valid config wins,
        // secondary entities render diagnostics only.
        let playhead_ns = ctx.time_ctrl.time_int().map_or(0, |t| t.as_i64());
        let timeline_is_playing = ctx.time_ctrl.play_state() == PlayState::Playing;
        let timeline = query.timeline;
        let store = ctx.recording();

        for summary in summaries.values() {
            let Some(config) = &summary.config else {
                continue;
            };

            let stream = ctx.store_context.memoizer(|cache: &mut AudioStreamCache| {
                cache
                    .audio_entry(
                        store,
                        &summary.entity_path,
                        timeline,
                        config.sample_rate,
                        config.sample_rate.max(1) as usize,
                    )
                    .ok()
            });
            let Some(stream) = stream else {
                continue;
            };

            let driving = state.drive(
                &summary.entity_path,
                &stream,
                playhead_ns,
                timeline_is_playing,
            );
            if driving {
                // Playback continually drains the ring, so we need repaints
                // even when the user is idle.
                ctx.egui_ctx().request_repaint();
            }
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            if summaries.is_empty() {
                ui.label("No AudioStream entities in this view.");
                return;
            }

            for summary in summaries.values() {
                draw_summary(ui, summary);
                ui.separator();
            }
        });

        Ok(())
    }
}

fn draw_summary(ui: &mut egui::Ui, summary: &AudioStreamSummary) {
    ui.strong(summary.entity_path.to_string());

    match &summary.config {
        Some(config) => draw_config(ui, config),
        None => {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Missing required config (codec / sample_rate / channel_count).",
            );
        }
    }

    let total_duration_text = match (summary.total_samples, &summary.config) {
        (Some(samples), Some(config)) if config.sample_rate > 0 => {
            let seconds = samples as f64 / f64::from(config.sample_rate);
            format!("{seconds:.2} s decoded")
        }
        (Some(samples), _) => format!("{samples} samples decoded"),
        (None, _) => "duration unknown (missing duration_samples)".to_owned(),
    };

    ui.horizontal_wrapped(|ui| {
        ui.label(format!("{} segments", summary.segment_count));
        ui.separator();
        ui.label(total_duration_text);
        if summary.discontinuities > 0 {
            ui.separator();
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("{} discontinuities", summary.discontinuities),
            );
        }
        if summary.out_of_order > 0 {
            ui.separator();
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("{} out-of-order", summary.out_of_order),
            );
        }
        if let Some(gaps) = summary.sequence_gaps {
            ui.separator();
            if gaps > 0 {
                ui.colored_label(ui.visuals().warn_fg_color, format!("{gaps} sequence gaps"));
            } else {
                ui.label("sequence contiguous");
            }
        }
    });

    if summary.segment_count == 0 {
        ui.weak("No chunks logged on the current timeline.");
    } else if let Some((first, last)) = summary.pts_range {
        ui.weak(format!(
            "pts range: {} .. {}",
            first.as_i64(),
            last.as_i64()
        ));
    }
}

fn draw_config(ui: &mut egui::Ui, config: &AudioStreamConfig) {
    let codec_label = match config.codec {
        components::AudioCodec::Opus => "Opus",
        components::AudioCodec::Flac => "FLAC",
    };
    ui.horizontal(|ui| {
        ui.label(format!(
            "{codec_label} · {} Hz · {} ch",
            config.sample_rate, config.channel_count
        ));
    });
}
