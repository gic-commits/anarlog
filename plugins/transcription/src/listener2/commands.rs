use std::str::FromStr;

use hypr_transcription_core::listener2 as core;
use sqlx::Row;
use tauri::Manager;

use crate::TranscriptionParams;
use crate::listener2::Listener2PluginExt;

#[tauri::command]
#[specta::specta]
pub async fn start_transcription<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    params: TranscriptionParams,
) -> Result<(), String> {
    tracing::info!(
        "[DEBUG] start_transcription command: session_id={} provider={:?} model={:?} progressive_batch={} file_path={} base_url={}",
        params.session_id,
        params.provider,
        params.model,
        params.progressive_batch,
        params.file_path,
        params.base_url,
    );

    let result = app.listener2().start_transcription(params).await;

    if let Err(ref e) = result {
        tracing::error!("[DEBUG] start_transcription failed: {}", e);
    } else {
        tracing::info!("[DEBUG] start_transcription returned Ok");
    }

    result.map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn stop_transcription<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
) -> Result<(), String> {
    app.listener2().stop_transcription(session_id).await;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn parse_subtitle<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    path: String,
) -> Result<core::Subtitle, String> {
    app.listener2().parse_subtitle(path)
}

#[tauri::command]
#[specta::specta]
pub async fn export_to_vtt<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
    words: Vec<core::VttWord>,
) -> Result<String, String> {
    app.listener2().export_to_vtt(session_id, words)
}

#[tauri::command]
#[specta::specta]
pub async fn run_denoise<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    params: core::DenoiseParams,
) -> Result<(), String> {
    app.listener2()
        .run_denoise(params)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn is_supported_languages_batch<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
    provider: String,
    model: Option<String>,
    languages: Vec<String>,
) -> Result<bool, String> {
    let languages_parsed = languages
        .iter()
        .map(|s| hypr_language::Language::from_str(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("unknown_language: {}", e))?;

    core::is_supported_languages_batch(&provider, model.as_deref(), &languages_parsed)
}

#[tauri::command]
#[specta::specta]
pub async fn suggest_providers_for_languages_batch<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
    languages: Vec<String>,
) -> Result<Vec<String>, String> {
    let languages_parsed = languages
        .iter()
        .map(|s| hypr_language::Language::from_str(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("unknown_language: {}", e))?;

    Ok(core::suggest_providers_for_languages_batch(
        &languages_parsed,
    ))
}

#[tauri::command]
#[specta::specta]
pub async fn list_documented_language_codes_batch<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
) -> Result<Vec<String>, String> {
    Ok(core::list_documented_language_codes_batch())
}

#[tauri::command]
#[specta::specta]
pub async fn list_progressive_batch_jobs<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
) -> Result<serde_json::Value, String> {
    let db_plugin = app.state::<tauri_plugin_db::ManagedState>();
    let rows = sqlx::query(
        "SELECT id, session_id, status, provider, model, base_url, language,
                segment_duration_ms, total_segments, completed_segments, failed_segments,
                abandoned_segments, created_at, updated_at, completed_at, error
         FROM progressive_batch_jobs
         WHERE session_id = ?1
         ORDER BY created_at DESC",
    )
    .bind(&session_id)
    .fetch_all(db_plugin.pool())
    .await
    .map_err(|e| e.to_string())?;

    let jobs: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.get::<String, _>("id"),
                "sessionId": row.get::<String, _>("session_id"),
                "status": row.get::<String, _>("status"),
                "provider": row.get::<String, _>("provider"),
                "model": row.get::<String, _>("model"),
                "baseUrl": row.get::<String, _>("base_url"),
                "language": row.get::<String, _>("language"),
                "segmentDurationMs": row.get::<i64, _>("segment_duration_ms"),
                "totalSegments": row.get::<i64, _>("total_segments"),
                "completedSegments": row.get::<i64, _>("completed_segments"),
                "failedSegments": row.get::<i64, _>("failed_segments"),
                "abandonedSegments": row.get::<i64, _>("abandoned_segments"),
                "createdAt": row.get::<String, _>("created_at"),
                "updatedAt": row.get::<String, _>("updated_at"),
                "completedAt": row.get::<Option<String>, _>("completed_at"),
                "error": row.get::<Option<String>, _>("error"),
            })
        })
        .collect();

    Ok(serde_json::json!(jobs))
}

#[tauri::command]
#[specta::specta]
pub async fn continue_progressive_batch<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    session_id: String,
    file_path: String,
    api_key: String,
) -> Result<crate::TranscriptionOutput, String> {
    let db_plugin = app.state::<tauri_plugin_db::ManagedState>();
    let pool = db_plugin.pool();
    let job = hypr_db_app::get_progressive_batch_job(pool, &session_id)
        .await
        .map_err(|e| e.to_string())?
        .filter(|j| j.status == "interrupted" || j.status == "partial")
        .ok_or_else(|| format!("no interrupted/partial job found for session {session_id}"))?;
    let segments =
        hypr_db_app::list_completed_progressive_batch_segments(db_plugin.pool(), &job.id)
            .await
            .map_err(|e| e.to_string())?;

    let mut completed: Vec<core::PersistedCompletedSegment> = Vec::new();
    for seg in &segments {
        let response_json = seg
            .response_json
            .as_deref()
            .ok_or_else(|| format!("segment {} has no response data", seg.id))?;
        let response: owhisper_interface::batch::Response =
            serde_json::from_str(response_json).map_err(|e| e.to_string())?;
        completed.push(core::PersistedCompletedSegment {
            index: seg.segment_index as usize,
            global_start_ms: seg.global_start_ms,
            response,
        });
    }

    let language = if job.language.is_empty() {
        vec![]
    } else {
        vec![
            job.language
                .parse::<hypr_language::Language>()
                .map_err(|e| format!("{e}"))?,
        ]
    };

    let params = crate::TranscriptionParams {
        session_id: session_id.clone(),
        provider: job
            .provider
            .parse::<core::BatchProvider>()
            .map_err(|e| format!("invalid provider: {e}"))?,
        file_path,
        model: if job.model.is_empty() {
            None
        } else {
            Some(job.model.clone())
        },
        base_url: job.base_url.clone(),
        api_key,
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
    };

    let output = app
        .listener2()
        .continue_transcription(params, completed)
        .await
        .map_err(|e| e.to_string())?;

    Ok(crate::TranscriptionOutput {
        session_id: output.session_id,
        mode: output.mode,
        response: output.response,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn mark_interrupted_jobs<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<serde_json::Value>, String> {
    let db_plugin = app.state::<tauri_plugin_db::ManagedState>();
    let pool = db_plugin.pool();

    let interrupted = hypr_db_app::mark_interrupted_progressive_batch_jobs(pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(interrupted
        .iter()
        .map(|job| {
            serde_json::json!({
                "id": job.id,
                "sessionId": job.session_id,
                "status": job.status,
            })
        })
        .collect())
}
