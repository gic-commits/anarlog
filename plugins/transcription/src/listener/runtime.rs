use std::sync::Arc;

use hypr_storage::StorageRuntime;
use hypr_transcription_core::listener::ListenerRuntime;
use hypr_transcription_core::listener2 as core;
use hypr_transcription_core::listener2::BatchProvider;
use hypr_transcription_core::listener2::BatchRuntime;
use ractor::{ActorRef, call_t, registry};
use tauri::Manager;
use tauri_plugin_settings::SettingsPluginExt;
use tauri_specta::Event;

use crate::{CaptureDataEvent, CaptureLifecycleEvent, CaptureStatusEvent, SessionStateCache};
use hypr_audio_utils::Source as _;
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
        audio_duration_ms: None,
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
        let sessions_base = self
            .vault_base()
            .map(|base| base.join("sessions"))
            .unwrap_or_else(|_| std::env::temp_dir());

        tauri::async_runtime::spawn(async move {
            let config = build_progressive_batch_config(&params);

            // Live path persists job/segment rows so coverage-incomplete
            // recordings can be continued. `persist_batch_event` needs a
            // BatchParams; construct a minimal one from the live params.
            let persist_params = core::BatchParams {
                session_id: params.session_id.clone(),
                provider: config.provider,
                file_path: String::new(),
                model: config.model.clone(),
                base_url: config.base_url.clone(),
                api_key: config.api_key.clone(),
                languages: params
                    .language
                    .as_deref()
                    .and_then(|l| l.parse().ok())
                    .into_iter()
                    .collect(),
                segment_duration_ms: Some(config.segment_duration_ms),
                overlap_ms: Some(config.overlap_ms),
                max_concurrency: Some(config.max_concurrency as u32),
                ..Default::default()
            };
            let runtime = Arc::new(LiveBatchRuntime {
                app: app.clone(),
                persist: Some(persist_params),
            });
            let segments_dir = config.session_dir.join("progressive-batch");
            let manager = Arc::new(tokio::sync::Mutex::new(
                core::ProgressiveBatchManager::new(config).with_runtime(runtime.clone()),
            ));
            tracing::info!(
                session_id = %params.session_id,
                segments_dir = %segments_dir.display(),
                "[live] progressive manager ready, seg wavs will be written under segments_dir"
            );

            // Set up diarization engine if enabled
            let diarization_engine = if params.diarization_enabled {
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

            // Diarization (embedding + clustering) is expensive and must NOT
            // run on the PCM consumption loop — a long embedding/cluster pass
            // would block `rx.recv()` and overflow the source PCM channel
            // (dropping frames → coverage gaps). Feed VAD segments into a
            // dedicated channel and let a separate task drain them.
            let (dia_tx, mut dia_rx) = tokio::sync::mpsc::unbounded_channel::<
                hypr_pyannote_local::segmentation::Segment,
            >();
            let dia_engine = std::sync::Arc::new(tokio::sync::Mutex::new(diarization_engine));
            let dia_task = {
                let dia_engine = dia_engine.clone();
                tauri::async_runtime::spawn(async move {
                    while let Some(seg) = dia_rx.recv().await {
                        let mut eng = dia_engine.lock().await;
                        if let Some(ref mut e) = *eng {
                            if let Err(err) = e.feed_segments(std::slice::from_ref(&seg)) {
                                tracing::warn!(error = %err, "diarization feed_segments error");
                            }
                        }
                    }
                })
            };

            let mut rx = pcm_rx;
            while let Some(frame) = rx.recv().await {
                manager.lock().await.on_audio_frame(&frame);
                // Hand VAD segments to the diarization task (non-blocking).
                let vad_segments = manager.lock().await.take_vad_segments();
                for seg in vad_segments {
                    let _ = dia_tx.send(seg);
                }
            }

            // Close the diarization feed channel and wait for the background
            // task to drain everything we sent before finalizing.
            drop(dia_tx);
            let _ = dia_task.await;

            // PCM stream ended (recording stopped). The final audio file is
            // now on disk — apply its real duration so the stitcher can flag
            // a coverage-incomplete transcript if live frames were dropped.
            let session_dir = sessions_base.join(&params.session_id);
            let audio_path = ["audio.mp3", "audio.wav", "audio.ogg"]
                .into_iter()
                .map(|f| session_dir.join(f))
                .find(|p| p.exists());
            if let Some(audio) = audio_path.clone() {
                if let Ok(source) = hypr_audio_utils::source_from_path(&audio) {
                    if let Some(dur) = source.total_duration() {
                        manager
                            .lock()
                            .await
                            .set_audio_duration(Some((dur.as_secs_f64() * 1000.0) as u64));
                        tracing::info!(
                            session_id = %params.session_id,
                            audio_duration_ms = dur.as_secs_f64() * 1000.0,
                            "progressive_batch set audio duration from final file"
                        );
                    }
                }
            }

            let finish_result = {
                let mut mgr = manager.lock().await;
                mgr.finish().await
            };
            match finish_result {
                Ok(mut response) => {
                    // Annotate with speaker labels after final clustering.
                    // Drain any VAD segments flushed during finish() first.
                    {
                        let mut eng = dia_engine.lock().await;
                        if let Some(ref mut e) = *eng {
                            let tail = { manager.lock().await.take_vad_segments() };
                            if !tail.is_empty() {
                                if let Err(err) = e.feed_segments(&tail) {
                                    tracing::warn!(error = %err, "diarization tail feed error");
                                }
                            }
                            e.finalize();
                            if let Some(ch) = response.results.channels.first_mut() {
                                if let Some(alt) = ch.alternatives.first_mut() {
                                    for word in &mut alt.words {
                                        let mid = (word.start + word.end) / 2.0;
                                        word.speaker = e.speaker_at_time(mid);
                                    }
                                }
                            }
                            core::propagate_speaker_to_none(&mut response);
                        }
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

                    // B: auto-complete coverage gap. If the live stream dropped
                    // frames and the transcript stops well before the audio end,
                    // retry once via the file-based continue path (audio.mp3 is
                    // now on disk) to fill the missing tail. Failure is
                    // non-fatal — the job stays partial and the frontend offers
                    // Continue.
                    if response
                        .metadata
                        .get("coverage_incomplete")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        && let Some(audio) = audio_path.clone()
                        && let Some(filled) =
                            auto_complete_live(&app, &params.session_id, &audio, &params.api_key)
                                .await
                    {
                        response = filled;
                    }

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
    persist: Option<core::BatchParams>,
}

impl core::BatchRuntime for LiveBatchRuntime {
    fn emit(&self, event: core::BatchEvent) {
        // Persist job/segment rows so a coverage-incomplete live recording can
        // be continued later (frontend shows "Continue" for partial jobs).
        if let (Some(params), Some(app)) = (self.persist.clone(), Some(self.app.clone())) {
            let persist_event = event.clone();
            tauri::async_runtime::spawn(async move {
                crate::listener2::ext::persist_batch_event(app, persist_event, params).await;
            });
        }
        if let Err(e) = crate::TranscriptionEvent::from(event).emit(&self.app) {
            tracing::error!(?e, "failed to emit progressive batch event");
        }
    }
}

/// B: auto-complete a coverage-incomplete live recording by continuing the
/// persisted job against the final audio file. Runs exactly once; if it fails
/// or is still incomplete, the job stays partial and the frontend shows
/// Continue for the user to decide.
async fn auto_complete_live(
    app: &tauri::AppHandle,
    session_id: &str,
    audio_path: &std::path::Path,
    api_key: &str,
) -> Option<owhisper_interface::batch::Response> {
    let state = app.try_state::<tauri_plugin_db::ManagedState>()?;
    let pool = state.pool();

    let job = match hypr_db_app::get_progressive_batch_job(pool, session_id).await {
        Ok(Some(j)) if j.status == "partial" || j.status == "running" => j,
        Ok(_) => {
            tracing::info!(
                session_id = %session_id,
                "auto_complete skipped: no partial/running job"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto_complete: get job failed");
            return None;
        }
    };

    let segments = match hypr_db_app::list_completed_progressive_batch_segments(pool, &job.id).await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "auto_complete: list segments failed");
            return None;
        }
    };

    let mut completed: Vec<core::PersistedCompletedSegment> = Vec::new();
    for seg in &segments {
        let Some(response_json) = seg.response_json.as_deref() else {
            continue;
        };
        let Ok(response) = serde_json::from_str(response_json) else {
            continue;
        };
        completed.push(core::PersistedCompletedSegment {
            index: seg.segment_index as usize,
            global_start_ms: seg.global_start_ms,
            response,
        });
    }
    if completed.is_empty() {
        return None;
    }

    // Re-align live-completed segments onto the full-audio group indices.
    // Live groups were produced from a possibly frame-dropped PCM stream, so
    // their indices don't match what `continue_from_file` will generate by
    // replaying the full file. Aligning avoids wrong skip/dedup.
    match core::compute_full_audio_groups(audio_path, job.segment_duration_ms as u32) {
        Ok(full_groups) => {
            completed = core::align_completed_segments_to_full_audio(&completed, &full_groups);
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto_complete: compute full groups failed");
        }
    }
    if completed.is_empty() {
        return None;
    }

    let language = if job.language.is_empty() {
        vec![]
    } else {
        job.language
            .parse::<hypr_language::Language>()
            .ok()
            .into_iter()
            .collect()
    };

    let persist_params = core::BatchParams {
        session_id: session_id.to_string(),
        provider: job.provider.parse().ok()?,
        file_path: audio_path.to_string_lossy().into_owned(),
        model: (!job.model.is_empty()).then_some(job.model.clone()),
        base_url: job.base_url.clone(),
        api_key: api_key.to_string(),
        languages: language,
        keywords: vec![],
        num_speakers: None,
        min_speakers: None,
        max_speakers: None,
        progressive_batch: true,
        segment_duration_ms: Some(job.segment_duration_ms as u32),
        overlap_ms: Some(job.overlap_ms as u32),
        max_concurrency: Some(job.max_concurrency as u32),
        cjk_enabled: true,
        cjk_features: None,
        cjk_server_side: false,
        diarization_enabled: false,
        diarization_model: None,
        diarization_threshold: 0.35,
    };

    let runtime = Arc::new(LiveBatchRuntime {
        app: app.clone(),
        persist: Some(persist_params.clone()),
    });

    match core::continue_from_file(runtime, persist_params, completed).await {
        Ok(output) => {
            tracing::info!(
                session_id = %session_id,
                filled_words = output.response.results.channels.first()
                    .and_then(|c| c.alternatives.first())
                    .map(|a| a.words.len())
                    .unwrap_or(0),
                "auto_complete: live transcript filled via continue"
            );
            Some(output.response)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto_complete: continue failed");
            None
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
