use std::collections::HashMap;
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use hypr_transcription_core::listener2 as core;
use tauri_specta::Event;
use tokio::task::JoinHandle;

use crate::{
    BatchSessionControl, BatchSessionEntry, BatchSessionRegistry, BatchTerminalState,
    TranscriptionEvent, TranscriptionParams,
};

const BATCH_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Listener2<'a, R: tauri::Runtime, M: tauri::Manager<R>> {
    manager: &'a M,
    _runtime: std::marker::PhantomData<fn() -> R>,
}

impl<'a, R: tauri::Runtime, M: tauri::Manager<R>> Listener2<'a, R, M> {
    pub async fn start_transcription(
        &self,
        params: TranscriptionParams,
    ) -> Result<(), core::Error> {
        let state = self.manager.state::<crate::SharedState>();
        let guard = state.lock().await;
        let app = guard.app.clone();
        drop(guard);

        let registry = self
            .manager
            .state::<Arc<BatchSessionRegistry>>()
            .inner()
            .clone();
        let session_id = params.session_id.clone();
        let idle_timeout = batch_idle_timeout(&params);

        {
            let mut sessions = lock_batch_sessions(&registry)?;
            if let Some(control) = sessions.get(&session_id).map(|entry| entry.control.clone()) {
                let state = match lock_terminal_state(&control) {
                    Ok(state) => *state,
                    Err(error) => {
                        tracing::warn!(
                            "[DEBUG] lock_terminal_state failed, removing entry: {}",
                            error
                        );
                        let entry = sessions.remove(&session_id);
                        drop(sessions);

                        if let Some(entry) = entry {
                            abort_batch_entry(entry);
                        }
                        return Err(error);
                    }
                };

                tracing::info!(
                    "[DEBUG] existing session {} found with state {:?}",
                    session_id,
                    state,
                );

                if state == BatchTerminalState::Running {
                    tracing::error!("[DEBUG] session already running: {}", session_id);
                    return Err(core::Error::BatchError(
                        "session already running".to_string(),
                    ));
                }

                sessions.remove(&session_id);
            }
        }

        let (last_activity_tx, _) = tokio::sync::watch::channel(Instant::now());
        let control = Arc::new(BatchSessionControl {
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            last_activity_tx,
            terminal_state: std::sync::Mutex::new(BatchTerminalState::Running),
        });

        {
            let mut sessions = lock_batch_sessions(&registry)?;
            sessions.insert(
                session_id.clone(),
                BatchSessionEntry {
                    control: control.clone(),
                    abort_handle: None,
                },
            );
        }

        let batch_params: core::BatchParams = params.clone().into();

        let runtime = Arc::new(TauriBatchRuntime {
            app: app.clone(),
            control: control.clone(),
            params: batch_params,
        });

        let task = tokio::spawn({
            let runtime = runtime.clone();
            let registry = registry.clone();
            let control = control.clone();
            let session_id = session_id.clone();
            async move {
                tracing::info!(
                    "[DEBUG] core::run_batch starting for session {}",
                    session_id
                );
                let result = core::run_batch(runtime, params.into()).await;
                tracing::info!(
                    "[DEBUG] core::run_batch finished for session {}: {:?}",
                    session_id,
                    result.as_ref().err()
                );
                finish_batch_session(&registry, &session_id, &control);
            }
        });
        let abort_handle = task.abort_handle();

        let is_running = {
            let mut sessions = match lock_batch_sessions(&registry) {
                Ok(sessions) => sessions,
                Err(error) => {
                    abort_handle.abort();
                    return Err(error);
                }
            };
            let Some(entry) = sessions.get_mut(&session_id) else {
                abort_handle.abort();
                return Ok(());
            };

            if !Arc::ptr_eq(&entry.control, &control) {
                abort_handle.abort();
                return Err(core::Error::BatchError(
                    "session already running".to_string(),
                ));
            }

            entry.abort_handle = Some(abort_handle.clone());

            match lock_terminal_state(&control) {
                Ok(state) => *state == BatchTerminalState::Running,
                Err(error) => {
                    sessions.remove(&session_id);
                    abort_handle.abort();
                    return Err(error);
                }
            }
        };

        if !is_running {
            remove_batch_session(&registry, &session_id, &control);
            return Ok(());
        }

        if let Some(idle_timeout) = idle_timeout {
            spawn_idle_timeout_monitor(
                app,
                registry,
                session_id,
                control,
                abort_handle,
                idle_timeout,
            );
        }

        Ok(())
    }

    pub async fn stop_transcription(&self, session_id: String) {
        let state = self.manager.state::<crate::SharedState>();
        let guard = state.lock().await;
        let app = guard.app.clone();
        drop(guard);

        let registry = self
            .manager
            .state::<Arc<BatchSessionRegistry>>()
            .inner()
            .clone();
        stop_batch_session(&app, &registry, &session_id);
    }

    pub async fn continue_transcription(
        &self,
        params: TranscriptionParams,
        completed: Vec<core::PersistedCompletedSegment>,
    ) -> Result<core::BatchRunOutput, core::Error> {
        let state = self.manager.state::<crate::SharedState>();
        let guard = state.lock().await;
        let app = guard.app.clone();
        drop(guard);

        let (last_activity_tx, _) = tokio::sync::watch::channel(std::time::Instant::now());
        let control = Arc::new(BatchSessionControl {
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            last_activity_tx,
            terminal_state: std::sync::Mutex::new(BatchTerminalState::Running),
        });

        let batch_params: core::BatchParams = params.into();
        let runtime = Arc::new(TauriBatchRuntime {
            app: app.clone(),
            control,
            params: batch_params.clone(),
        });

        core::continue_from_file(runtime, batch_params, completed).await
    }

    pub async fn run_denoise(&self, params: core::DenoiseParams) -> Result<(), core::Error> {
        let state = self.manager.state::<crate::SharedState>();
        let guard = state.lock().await;
        let app = guard.app.clone();
        drop(guard);

        let runtime = Arc::new(TauriDenoiseRuntime { app });
        core::run_denoise(runtime, params).await
    }

    pub fn parse_subtitle(&self, path: String) -> Result<core::Subtitle, String> {
        core::parse_subtitle_from_path(path)
    }

    pub fn export_to_vtt(
        &self,
        session_id: String,
        words: Vec<core::VttWord>,
    ) -> Result<String, String> {
        use tauri_plugin_settings::SettingsPluginExt;

        let base = self
            .manager
            .settings()
            .vault_base()
            .map_err(|e| e.to_string())?;
        let session_dir = base.join("sessions").join(&session_id);

        std::fs::create_dir_all(&session_dir).map_err(|e| e.to_string())?;

        let vtt_path = session_dir.join("transcript.vtt");

        core::export_words_to_vtt_file(words, &vtt_path)?;
        Ok(vtt_path.to_string())
    }
}

pub trait Listener2PluginExt<R: tauri::Runtime> {
    fn listener2(&self) -> Listener2<'_, R, Self>
    where
        Self: tauri::Manager<R> + Sized;
}

impl<R: tauri::Runtime, T: tauri::Manager<R>> Listener2PluginExt<R> for T {
    fn listener2(&self) -> Listener2<'_, R, Self>
    where
        Self: Sized,
    {
        Listener2 {
            manager: self,
            _runtime: std::marker::PhantomData,
        }
    }
}

pub(crate) struct TauriBatchRuntime {
    app: tauri::AppHandle,
    control: Arc<BatchSessionControl>,
    params: core::BatchParams,
}

impl core::BatchRuntime for TauriBatchRuntime {
    // AppHandle's inner runtime is irrelevant for event emission
    // since it just serializes to JSON and sends via IPC.
    // The emit already works with any runtime. The app handle
    // is used only for event emission and state access, which
    // work through trait methods on AppHandle.
    //
    // Note: we store tauri::AppHandle (which is Wry-specific)
    // because the commands in this plugin are all registered
    // with tauri::Wry via the explicit turbofish in lib.rs.
    fn emit(&self, event: core::BatchEvent) {
        if !should_emit_event(&self.control, &event) {
            tracing::info!("[DEBUG] TauriBatchRuntime emit: skipped (terminal state)");
            return;
        }

        tracing::info!("[DEBUG] TauriBatchRuntime emit: event={:?}", event);

        if matches!(
            event,
            core::BatchEvent::BatchResponseStreamed { .. }
                | core::BatchEvent::BatchResponse { .. }
                | core::BatchEvent::BatchSegmentResult { .. }
        ) {
            let _ = self.control.last_activity_tx.send(Instant::now());
        }

        if let core::BatchEvent::BatchCompleted { .. } = event {
            return;
        }

        let app = self.app.clone();
        let persist_event = event.clone();
        let params = self.params.clone();
        tokio::spawn(async move {
            persist_batch_event(app, persist_event, params).await;
        });

        let _ = TranscriptionEvent::from(event).emit(&self.app);
    }
}

async fn persist_batch_event(
    app: tauri::AppHandle,
    event: core::BatchEvent,
    params: core::BatchParams,
) {
    use tauri::Manager;

    let state = match app.try_state::<tauri_plugin_db::ManagedState>() {
        Some(state) => state,
        None => {
            tracing::warn!("[batch_persist] db plugin not available");
            return;
        }
    };
    let pool = state.pool();

    match &event {
        core::BatchEvent::BatchStarted { session_id } => {
            let provider = params.provider.to_string();
            let model = params.model.clone().unwrap_or_default();
            let language = params
                .languages
                .first()
                .map(|l| l.to_string())
                .unwrap_or_default();

            if let Err(e) = sqlx::query(
                "INSERT OR IGNORE INTO progressive_batch_jobs
                 (id, session_id, status, provider, model, base_url, language,
                  segment_duration_ms, overlap_ms, max_concurrency)
                 VALUES (?1, ?2, 'running', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .bind(session_id)
            .bind(session_id)
            .bind(&provider)
            .bind(&model)
            .bind(&params.base_url)
            .bind(&language)
            .bind(params.segment_duration_ms.map(|v| v as i64))
            .bind(params.overlap_ms.map(|v| v as i64))
            .bind(params.max_concurrency.map(|v| v as i64))
            .execute(pool)
            .await
            {
                tracing::warn!("[batch_persist] create job failed: {e}");
            }
        }

        core::BatchEvent::BatchSegmentResult {
            session_id,
            segment_index,
            global_start_ms,
            response,
        } => {
            let segment_id = format!("{session_id}_{segment_index}");
            let response_json = match serde_json::to_string(response) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!("[batch_persist] serialize response failed: {e}");
                    return;
                }
            };

            if let Err(e) = sqlx::query(
                "INSERT OR REPLACE INTO progressive_batch_segments
                 (id, job_id, segment_index, global_start_ms, status, response_json, updated_at)
                 VALUES (
                   ?1, ?2, ?3, ?4, 'completed', ?5,
                   strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 )",
            )
            .bind(&segment_id)
            .bind(session_id)
            .bind(*segment_index as i64)
            .bind(*global_start_ms)
            .bind(&response_json)
            .execute(pool)
            .await
            {
                tracing::warn!("[batch_persist] upsert segment failed: {e}");
            }

            if let Err(e) = sqlx::query(
                "UPDATE progressive_batch_jobs
                 SET completed_segments = (
                   SELECT COUNT(*) FROM progressive_batch_segments
                   WHERE job_id = ?2 AND status = 'completed'
                 ),
                     total_segments = MAX(total_segments, ?3),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1",
            )
            .bind(session_id)
            .bind(session_id)
            .bind(*segment_index as i64 + 1)
            .execute(pool)
            .await
            {
                tracing::warn!("[batch_persist] update job count failed: {e}");
            }
        }

        core::BatchEvent::BatchResponse {
            session_id,
            response,
            ..
        } => {
            let has_abandoned = response
                .metadata
                .get("abandoned_segments")
                .and_then(|v| v.as_array())
                .is_some_and(|arr| !arr.is_empty());

            let status = if has_abandoned {
                "partial"
            } else {
                "completed"
            };

            if let Err(e) = sqlx::query(
                "UPDATE progressive_batch_jobs
                 SET status = ?2,
                     completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1 AND status = 'running'",
            )
            .bind(session_id)
            .bind(status)
            .execute(pool)
            .await
            {
                tracing::warn!("[batch_persist] mark complete failed: {e}");
            }
        }

        core::BatchEvent::BatchFailed {
            session_id,
            code,
            error,
        } => {
            if let Err(e) = sqlx::query(
                "UPDATE progressive_batch_jobs
                 SET status = 'failed',
                     error = ?2,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1",
            )
            .bind(session_id)
            .bind(format!("{code:?}: {error}"))
            .execute(pool)
            .await
            {
                tracing::warn!("[batch_persist] mark failed failed: {e}");
            }
        }

        core::BatchEvent::DiarizationStarted { session_id, total_segments } => {
            let model = params.diarization_model.clone().unwrap_or_default();
            let threshold = params.diarization_threshold;

            if let Err(e) = sqlx::query(
                "INSERT OR IGNORE INTO diarization_jobs
                 (id, session_id, status, model, threshold, total_segments)
                 VALUES (?1, ?2, 'running', ?3, ?4, ?5)",
            )
            .bind(session_id)
            .bind(session_id)
            .bind(&model)
            .bind(threshold as f64)
            .bind(*total_segments as i64)
            .execute(pool)
            .await
            {
                tracing::warn!("[diarization_persist] create job failed: {e}");
            }
        }

        core::BatchEvent::DiarizationSegmentResult {
            session_id,
            segment_index,
            speaker,
            response,
            ..
        } => {
            let segment_id = format!("{session_id}_{segment_index}");
            let response_json = match serde_json::to_string(response) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!("[diarization_persist] serialize response failed: {e}");
                    return;
                }
            };

            if let Err(e) = sqlx::query(
                "INSERT OR REPLACE INTO diarization_segments
                 (id, job_id, segment_index, speaker, status, response_json, updated_at)
                 VALUES (
                   ?1, ?2, ?3, ?4, 'completed', ?5,
                   strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 )",
            )
            .bind(&segment_id)
            .bind(session_id)
            .bind(*segment_index as i64)
            .bind(*speaker as i64)
            .bind(&response_json)
            .execute(pool)
            .await
            {
                tracing::warn!("[diarization_persist] upsert segment failed: {e}");
            }

            if let Err(e) = sqlx::query(
                "UPDATE diarization_jobs
                 SET completed_segments = (
                   SELECT COUNT(*) FROM diarization_segments
                   WHERE job_id = ?2 AND status = 'completed'
                 ),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1",
            )
            .bind(session_id)
            .bind(session_id)
            .execute(pool)
            .await
            {
                tracing::warn!("[diarization_persist] update job count failed: {e}");
            }
        }

        core::BatchEvent::BatchCompleted { .. }
        | core::BatchEvent::BatchResponseStreamed { .. } => {}
    }
}

struct TauriDenoiseRuntime {
    app: tauri::AppHandle,
}

impl core::DenoiseRuntime for TauriDenoiseRuntime {
    fn emit(&self, event: core::DenoiseEvent) {
        let _ = event.emit(&self.app);
    }
}

fn batch_lock_poisoned(name: &'static str) -> core::Error {
    tracing::error!(lock = name, "batch_session_lock_poisoned");
    core::Error::BatchError(format!("{name} poisoned"))
}

fn lock_batch_sessions(
    registry: &BatchSessionRegistry,
) -> Result<MutexGuard<'_, HashMap<String, BatchSessionEntry>>, core::Error> {
    registry
        .sessions
        .lock()
        .map_err(|_| batch_lock_poisoned("batch session registry"))
}

fn lock_terminal_state(
    control: &BatchSessionControl,
) -> Result<MutexGuard<'_, BatchTerminalState>, core::Error> {
    control
        .terminal_state
        .lock()
        .map_err(|_| batch_lock_poisoned("batch terminal state"))
}

fn should_emit_event(control: &BatchSessionControl, event: &core::BatchEvent) -> bool {
    let Ok(state) = lock_terminal_state(control) else {
        return false;
    };
    let state = *state;

    state == BatchTerminalState::Running
        || matches!(
            (state, event),
            (
                BatchTerminalState::Finished,
                core::BatchEvent::BatchResponse { .. }
            )
        )
}

fn mark_terminal_state(control: &BatchSessionControl, next: BatchTerminalState) -> bool {
    let Ok(mut state) = lock_terminal_state(control) else {
        return false;
    };

    if *state != BatchTerminalState::Running {
        return false;
    }
    *state = next;
    control.cancellation_token.cancel();
    true
}

fn finish_batch_session(
    registry: &Arc<BatchSessionRegistry>,
    session_id: &str,
    control: &Arc<BatchSessionControl>,
) {
    {
        if let Ok(mut state) = lock_terminal_state(control) {
            if *state == BatchTerminalState::Running {
                *state = BatchTerminalState::Finished;
                control.cancellation_token.cancel();
            }
        }
    }

    remove_batch_session(registry, session_id, control);
}

fn remove_batch_session(
    registry: &Arc<BatchSessionRegistry>,
    session_id: &str,
    control: &Arc<BatchSessionControl>,
) {
    let Ok(mut sessions) = lock_batch_sessions(registry) else {
        return;
    };

    let should_remove = sessions
        .get(session_id)
        .is_some_and(|entry| Arc::ptr_eq(&entry.control, control));
    if should_remove {
        sessions.remove(session_id);
    }
}

fn abort_batch_entry(entry: BatchSessionEntry) {
    if let Some(abort_handle) = entry.abort_handle {
        abort_handle.abort();
    }
}

fn stop_batch_session(
    app: &tauri::AppHandle,
    registry: &Arc<BatchSessionRegistry>,
    session_id: &str,
) {
    let entry = {
        let Ok(mut sessions) = lock_batch_sessions(registry) else {
            return;
        };

        sessions.remove(session_id)
    };

    let Some(entry) = entry else {
        return;
    };

    if mark_terminal_state(&entry.control, BatchTerminalState::Stopped) {
        let _ = TranscriptionEvent::Stopped {
            session_id: session_id.to_string(),
        }
        .emit(app);
    }

    abort_batch_entry(entry);
}

fn batch_idle_timeout(params: &TranscriptionParams) -> Option<Duration> {
    let batch_params: core::BatchParams = params.clone().into();

    core::expects_progressive_batch(&batch_params).then_some(BATCH_IDLE_TIMEOUT)
}

fn spawn_idle_timeout_monitor(
    app: tauri::AppHandle,
    registry: Arc<BatchSessionRegistry>,
    session_id: String,
    control: Arc<BatchSessionControl>,
    abort_handle: tokio::task::AbortHandle,
    idle_timeout: Duration,
) -> JoinHandle<()> {
    let mut activity_rx = control.last_activity_tx.subscribe();

    tokio::spawn(async move {
        loop {
            let deadline = *activity_rx.borrow() + idle_timeout;
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
            tokio::pin!(sleep);

            tokio::select! {
                _ = control.cancellation_token.cancelled() => return,
                _ = &mut sleep => {
                    if !mark_terminal_state(&control, BatchTerminalState::TimedOut) {
                        return;
                    }

                    remove_batch_session(&registry, &session_id, &control);
                    let _ = TranscriptionEvent::Failed {
                        session_id: session_id.clone(),
                        code: core::BatchErrorCode::TimedOut,
                        error: format!(
                            "Transcription timed out after {} seconds without progress.",
                            idle_timeout.as_secs()
                        ),
                    }
                    .emit(&app);
                    abort_handle.abort();
                    return;
                }
                changed = activity_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_control() -> Arc<BatchSessionControl> {
        let (last_activity_tx, _) = tokio::sync::watch::channel(Instant::now());
        Arc::new(BatchSessionControl {
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            last_activity_tx,
            terminal_state: std::sync::Mutex::new(BatchTerminalState::Running),
        })
    }

    fn make_registry(control: Arc<BatchSessionControl>) -> Arc<BatchSessionRegistry> {
        Arc::new(BatchSessionRegistry {
            sessions: std::sync::Mutex::new(std::collections::HashMap::from([(
                "session-1".to_string(),
                BatchSessionEntry {
                    control,
                    abort_handle: None,
                },
            )])),
        })
    }

    fn poison_terminal_state(control: Arc<BatchSessionControl>) {
        assert!(
            std::thread::spawn(move || {
                let _guard = control.terminal_state.lock().unwrap();
                panic!("poison terminal state");
            })
            .join()
            .is_err()
        );
    }

    fn poison_registry(registry: Arc<BatchSessionRegistry>) {
        assert!(
            std::thread::spawn(move || {
                let _guard = registry.sessions.lock().unwrap();
                panic!("poison registry");
            })
            .join()
            .is_err()
        );
    }

    fn transcription_params(
        provider: core::BatchProvider,
        base_url: &str,
        model: Option<&str>,
    ) -> TranscriptionParams {
        TranscriptionParams {
            session_id: "session-1".to_string(),
            provider,
            file_path: "/tmp/audio.wav".to_string(),
            model: model.map(ToOwned::to_owned),
            base_url: base_url.to_string(),
            api_key: "key".to_string(),
            languages: vec![hypr_language::ISO639::En.into()],
            keywords: vec![],
            num_speakers: None,
            min_speakers: None,
            max_speakers: None,
            progressive_batch: false,
            segment_duration_ms: None,
            overlap_ms: None,
            max_concurrency: None,
            cjk_enabled: true,
            cjk_features: None,
            cjk_server_side: false,
            diarization_enabled: false,
            diarization_model: None,
            diarization_threshold: 0.35,
        }
    }

    #[test]
    fn mark_terminal_state_only_transitions_once() {
        let control = make_control();

        assert!(mark_terminal_state(&control, BatchTerminalState::Stopped));
        assert!(!mark_terminal_state(&control, BatchTerminalState::TimedOut));
        assert_eq!(
            *control
                .terminal_state
                .lock()
                .expect("batch terminal state poisoned"),
            BatchTerminalState::Stopped,
        );
    }

    #[test]
    fn should_emit_event_stops_after_terminal_transition() {
        let control = make_control();
        let event = core::BatchEvent::BatchStarted {
            session_id: "session-1".to_string(),
        };

        assert!(should_emit_event(&control, &event));
        assert!(mark_terminal_state(&control, BatchTerminalState::Stopped));
        assert!(!should_emit_event(&control, &event));
    }

    #[test]
    fn lock_terminal_state_returns_batch_error_when_poisoned() {
        let control = make_control();
        poison_terminal_state(control.clone());

        match lock_terminal_state(&control) {
            Err(core::Error::BatchError(message)) => {
                assert!(message.contains("batch terminal state poisoned"));
            }
            _ => panic!("expected terminal state poison to return BatchError"),
        }
    }

    #[test]
    fn lock_batch_sessions_returns_batch_error_when_poisoned() {
        let registry = make_registry(make_control());
        poison_registry(registry.clone());

        match lock_batch_sessions(&registry) {
            Err(core::Error::BatchError(message)) => {
                assert!(message.contains("batch session registry poisoned"));
            }
            _ => panic!("expected registry poison to return BatchError"),
        }
    }

    #[test]
    fn poisoned_terminal_state_stops_emit_and_transition_without_panic() {
        let control = make_control();
        let event = core::BatchEvent::BatchStarted {
            session_id: "session-1".to_string(),
        };
        poison_terminal_state(control.clone());

        assert!(!should_emit_event(&control, &event));
        assert!(!mark_terminal_state(&control, BatchTerminalState::Stopped));
    }

    #[test]
    fn finish_batch_session_removes_entry_when_terminal_state_is_poisoned() {
        let control = make_control();
        let registry = make_registry(control.clone());
        poison_terminal_state(control.clone());

        finish_batch_session(&registry, "session-1", &control);

        assert!(
            !registry
                .sessions
                .lock()
                .expect("batch session registry poisoned")
                .contains_key("session-1")
        );
    }

    #[test]
    fn finish_batch_session_removes_matching_registry_entry() {
        let control = make_control();
        let registry = make_registry(control.clone());

        finish_batch_session(&registry, "session-1", &control);

        assert!(
            !registry
                .sessions
                .lock()
                .expect("batch session registry poisoned")
                .contains_key("session-1")
        );
    }

    #[tokio::test]
    async fn abort_batch_entry_cancels_background_task() {
        let task = tokio::spawn(std::future::pending::<()>());

        abort_batch_entry(BatchSessionEntry {
            control: make_control(),
            abort_handle: Some(task.abort_handle()),
        });

        assert!(
            task.await
                .expect_err("batch task should be aborted")
                .is_cancelled()
        );
    }

    #[test]
    fn batch_idle_timeout_skips_direct_cloud_batch() {
        let params = transcription_params(
            core::BatchProvider::Hyprnote,
            "https://api.char.com/stt",
            None,
        );

        assert_eq!(batch_idle_timeout(&params), None);
    }

    #[test]
    fn batch_idle_timeout_skips_cloud_am_batch() {
        let params =
            transcription_params(core::BatchProvider::Am, "https://api.char.com/stt", None);

        assert_eq!(batch_idle_timeout(&params), None);
    }

    #[test]
    fn batch_idle_timeout_applies_to_local_am_batch() {
        let params =
            transcription_params(core::BatchProvider::Am, "http://localhost:50060/v1", None);

        assert_eq!(batch_idle_timeout(&params), Some(BATCH_IDLE_TIMEOUT));
    }
}
