use std::sync::Arc;

use hypr_transcription_core::listener::ListenerRuntime;
use hypr_transcription_core::listener2 as core;
use hypr_transcription_core::listener2::BatchProvider;
use hypr_transcription_core::listener2::BatchRuntime;
use ractor::{ActorRef, call_t, registry};
use tauri_plugin_settings::SettingsPluginExt;
use tauri_specta::Event;

use crate::{CaptureDataEvent, CaptureLifecycleEvent, CaptureStatusEvent, SessionStateCache};
use hypr_transcription_core::listener::State as RootState;
use hypr_transcription_core::listener::actors::{RootActor, RootMsg};

pub struct TauriRuntime {
    pub app: tauri::AppHandle,
    pub session_state_cache: SessionStateCache,
}

fn build_progressive_batch_config(
    params: &hypr_transcription_core::listener::ProgressiveBatchParams,
) -> core::ProgressiveBatchConfig {
    core::ProgressiveBatchConfig {
        session_id: params.session_id.clone(),
        sample_rate: params.sample_rate,
        segment_duration_ms: params.segment_duration_ms.unwrap_or(30000),
        // VAD groups are non-overlapping; the stitcher must not dedup.
        overlap_ms: 0,
        max_concurrency: 2,
        base_url: params.base_url.clone(),
        api_key: params.api_key.clone(),
        model: Some(params.model.clone()),
        language: params.language.clone(),
        provider: params
            .provider
            .as_deref()
            .and_then(|p| p.parse().ok())
            .unwrap_or(BatchProvider::OpenAI),
        session_dir: std::env::temp_dir().join(format!("progressive-{}", params.session_id)),
        vad_groups: true,
        collect_vad_segments: params.diarization_enabled,
    }
}

impl hypr_storage::StorageRuntime for TauriRuntime {
    fn global_base(&self) -> Result<std::path::PathBuf, hypr_storage::Error> {
        self.app
            .settings()
            .global_base()
            .map(|p| p.into_std_path_buf())
            .map_err(|_| hypr_storage::Error::DataDirUnavailable)
    }

    fn vault_base(&self) -> Result<std::path::PathBuf, hypr_storage::Error> {
        self.app
            .settings()
            .vault_base()
            .map(|p| p.into_std_path_buf())
            .map_err(|_| hypr_storage::Error::DataDirUnavailable)
    }
}

impl ListenerRuntime for TauriRuntime {
    fn emit_lifecycle(&self, event: hypr_transcription_core::listener::SessionLifecycleEvent) {
        use tauri_plugin_tray::TrayPluginExt;
        match &event {
            hypr_transcription_core::listener::SessionLifecycleEvent::Active { error, .. } => {
                let _ = self.app.tray().set_start_disabled(true);
                let _ = self.app.tray().set_degraded(error.is_some());
                let _ = self.app.tray().set_recording(true);
            }
            hypr_transcription_core::listener::SessionLifecycleEvent::Inactive { .. } => {
                let app = self.app.clone();
                tauri::async_runtime::spawn(async move {
                    match current_root_state().await {
                        RootState::Active => {}
                        RootState::Finalizing => {
                            let _ = app.tray().set_start_disabled(false);
                            let _ = app.tray().set_recording(false);
                        }
                        RootState::Inactive => {
                            let _ = app.tray().set_start_disabled(false);
                            let _ = app.tray().set_recording(false);
                            let _ = app.tray().set_degraded(false);
                        }
                    }
                });
            }
            hypr_transcription_core::listener::SessionLifecycleEvent::Finalizing { .. } => {}
        }

        let capture_event = match event {
            hypr_transcription_core::listener::SessionLifecycleEvent::Active {
                session_id,
                requested_transcription_mode,
                current_transcription_mode,
                error,
            } => {
                let requested_live_transcription = requested_transcription_mode
                    == hypr_transcription_core::listener::TranscriptionMode::Live;
                let live_transcription_active = current_transcription_mode
                    == hypr_transcription_core::listener::TranscriptionMode::Live;
                if let Ok(mut cache) = self.session_state_cache.lock() {
                    cache.insert(
                        session_id.clone(),
                        (requested_live_transcription, live_transcription_active),
                    );
                }
                CaptureLifecycleEvent::Started {
                    session_id,
                    requested_live_transcription,
                    live_transcription_active,
                    degraded: error,
                }
            }
            hypr_transcription_core::listener::SessionLifecycleEvent::Finalizing { session_id } => {
                CaptureLifecycleEvent::Finalizing { session_id }
            }
            hypr_transcription_core::listener::SessionLifecycleEvent::Inactive {
                session_id,
                audio_path,
                error,
            } => {
                let (requested_live_transcription, live_transcription_active) = self
                    .session_state_cache
                    .lock()
                    .ok()
                    .and_then(|mut cache| cache.remove(&session_id))
                    .unwrap_or((false, false));

                CaptureLifecycleEvent::Stopped {
                    session_id,
                    audio_path,
                    requested_live_transcription,
                    live_transcription_active,
                    error,
                }
            }
        };

        if let Err(error) = capture_event.emit(&self.app) {
            tracing::error!(?error, "failed_to_emit_lifecycle_event");
        }
    }

    fn emit_progress(&self, event: hypr_transcription_core::listener::SessionProgressEvent) {
        if let Err(error) = CaptureStatusEvent::from(event).emit(&self.app) {
            tracing::error!(?error, "failed_to_emit_progress_event");
        }
    }

    fn emit_error(&self, event: hypr_transcription_core::listener::SessionErrorEvent) {
        if let Err(error) = CaptureStatusEvent::from(event).emit(&self.app) {
            tracing::error!(?error, "failed_to_emit_error_event");
        }
    }

    fn emit_data(&self, event: hypr_transcription_core::listener::SessionDataEvent) {
        if let Err(error) = CaptureDataEvent::from(event).emit(&self.app) {
            tracing::error!(?error, "failed_to_emit_data_event");
        }
    }

    fn start_progressive_batch_stream(
        &self,
        params: hypr_transcription_core::listener::ProgressiveBatchParams,
        pcm_rx: tokio::sync::mpsc::Receiver<Arc<[f32]>>,
    ) {
        let app = self.app.clone();

        tauri::async_runtime::spawn(async move {
            let config = build_progressive_batch_config(&params);

            let runtime = Arc::new(LiveBatchRuntime { app: app.clone() });
            let segments_dir = config.session_dir.join("progressive-batch");
            let mut manager =
                core::ProgressiveBatchManager::new(config).with_runtime(runtime.clone());
            tracing::info!(
                session_id = %params.session_id,
                segments_dir = %segments_dir.display(),
                "[live] progressive manager ready, seg wavs will be written under segments_dir"
            );

            // Set up diarization engine if enabled
            let mut diarization_engine = if params.diarization_enabled {
                match hypr_pyannote_local::incremental_diarization::IncrementalDiarizationEngine::new(
                    hypr_pyannote_local::incremental_diarization::IncrementalDiarizationConfig {
                        sample_rate: params.sample_rate,
                        model_path: params.diarization_model.clone(),
                        threshold: params.diarization_threshold,
                        recluster_interval: 5,
                        ..Default::default()
                    },
                ) {
                    Ok(engine) => {
                        tracing::info!(
                            session_id = %params.session_id,
                            "diarization engine initialized for live stream"
                        );
                        Some(engine)
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id = %params.session_id,
                            error = %e,
                            "failed to initialize diarization engine, continuing without"
                        );
                        None
                    }
                }
            } else {
                None
            };

            let mut rx = pcm_rx;
            while let Some(frame) = rx.recv().await {
                manager.on_audio_frame(&frame);
                // Feed diarization with the same VAD segments the segmenter
                // grouped on (via the stream's shared VAD), matching the file
                // re-transcribe path's `feed_segments`.
                if let Some(ref mut eng) = diarization_engine {
                    let vad_segments = manager.take_vad_segments();
                    if !vad_segments.is_empty() {
                        if let Err(e) = eng.feed_segments(&vad_segments) {
                            tracing::warn!(error = %e, "diarization feed_segments error");
                        }
                    }
                }
            }

            match manager.finish().await {
                Ok(mut response) => {
                    // Annotate with speaker labels after final clustering.
                    // Drain any VAD segments flushed during finish() first.
                    if let Some(ref mut eng) = diarization_engine {
                        let tail = manager.take_vad_segments();
                        if !tail.is_empty() {
                            if let Err(e) = eng.feed_segments(&tail) {
                                tracing::warn!(error = %e, "diarization tail feed error");
                            }
                        }
                        eng.finalize();
                        if let Some(ch) = response.results.channels.first_mut() {
                            if let Some(alt) = ch.alternatives.first_mut() {
                                for word in &mut alt.words {
                                    let mid = (word.start + word.end) / 2.0;
                                    word.speaker = eng.speaker_at_time(mid);
                                }
                            }
                        }
                        core::propagate_speaker_to_none(&mut response);
                    }

                    let total_words = response
                        .results
                        .channels
                        .first()
                        .and_then(|c| c.alternatives.first())
                        .map(|a| a.words.len())
                        .unwrap_or(0);
                    tracing::info!(
                        session_id = %params.session_id,
                        total_words,
                        "progressive_batch session finished"
                    );

                    runtime.emit(core::BatchEvent::BatchResponse {
                        session_id: params.session_id.clone(),
                        response,
                        mode: core::BatchRunMode::Streamed,
                    });
                }
                Err(e) => {
                    tracing::error!(
                        session_id = %params.session_id,
                        error = %e,
                        "progressive_batch session failed"
                    );

                    runtime.emit(core::BatchEvent::BatchFailed {
                        session_id: params.session_id.clone(),
                        code: core::BatchErrorCode::ProgressiveBatchFailed,
                        error: e.to_string(),
                    });
                }
            }
        });
    }
}

struct LiveBatchRuntime {
    app: tauri::AppHandle,
}

impl core::BatchRuntime for LiveBatchRuntime {
    fn emit(&self, event: core::BatchEvent) {
        if let Err(e) = crate::TranscriptionEvent::from(event).emit(&self.app) {
            tracing::error!(?e, "failed to emit progressive batch event");
        }
    }
}

async fn current_root_state() -> RootState {
    let Some(cell) = registry::where_is(RootActor::name()) else {
        return RootState::Inactive;
    };

    let actor: ActorRef<RootMsg> = cell.into();
    call_t!(actor, RootMsg::GetState, 100).unwrap_or(RootState::Inactive)
}

#[cfg(test)]
mod tests {
    use super::build_progressive_batch_config;
    use hypr_transcription_core::listener::ProgressiveBatchParams;
    use hypr_transcription_core::listener2::BatchProvider;

    fn params() -> ProgressiveBatchParams {
        ProgressiveBatchParams {
            session_id: "s1".to_string(),
            base_url: "http://localhost:8080/v1".to_string(),
            api_key: "test-key".to_string(),
            model: "faster-whisper-small".to_string(),
            language: None,
            sample_rate: 16000,
            segment_duration_ms: None,
            diarization_enabled: false,
            diarization_model: None,
            diarization_threshold: 0.35,
            provider: None,
        }
    }

    #[test]
    fn config_defaults_segment_duration_to_30000() {
        let cfg = build_progressive_batch_config(&params());
        assert_eq!(cfg.segment_duration_ms, 30000);
    }

    #[test]
    fn config_honors_configured_segment_duration() {
        let mut p = params();
        p.segment_duration_ms = Some(60000);
        let cfg = build_progressive_batch_config(&p);
        assert_eq!(cfg.segment_duration_ms, 60000);
    }

    #[test]
    fn config_resolves_groq_provider() {
        let mut p = params();
        p.provider = Some("groq".to_string());
        let cfg = build_progressive_batch_config(&p);
        assert_eq!(cfg.provider, BatchProvider::Groq);
    }

    #[test]
    fn config_falls_back_to_openai_for_unknown_provider() {
        let mut p = params();
        p.provider = Some("not-a-provider".to_string());
        let cfg = build_progressive_batch_config(&p);
        assert_eq!(cfg.provider, BatchProvider::OpenAI);
    }
}
