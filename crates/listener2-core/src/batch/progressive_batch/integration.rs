use std::sync::Arc;

use hypr_audio_utils::Source;

use crate::batch::{BatchParams, BatchRunMode, BatchRunOutput};
use crate::{BatchEvent, BatchRuntime};

use super::*;

fn mime_type_for_extension(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "audio/mp4",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("webm") => "audio/webm",
        Some("ogg") => "audio/ogg",
        Some("flac") => "audio/flac",
        _ => "application/octet-stream",
    }
}

pub async fn run_progressive_batch_from_file(
    runtime: Arc<dyn BatchRuntime>,
    params: BatchParams,
    _sample_rate: u32,
    _channels: u8,
) -> crate::Result<BatchRunOutput> {
    let session_id = params.session_id.clone();
    let file_path = params.file_path.clone();

    let source = hypr_audio_utils::source_from_path(&file_path).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to open audio file: {e}"),
        }
    })?;

    let file_sample_rate: u32 = source.sample_rate().into();
    let file_channels: usize = source.channels().get().into();

    let segment_duration_ms = params.segment_duration_ms.unwrap_or(30000);

    let duration_secs = source
        .total_duration()
        .map(|d| d.as_secs_f64())
        .unwrap_or(f64::MAX);

    // Audio shorter than one segment → POST original file directly.
    if duration_secs < segment_duration_ms as f64 / 1000.0 {
        return submit_file_direct(
            runtime,
            std::path::Path::new(&file_path),
            &params,
            &session_id,
        )
        .await;
    }

    let session_dir = std::env::temp_dir().join(format!("progressive-batch-{session_id}"));
    let _ = std::fs::create_dir_all(&session_dir);

    let language = params.languages.first().map(|l| l.to_string());

    let config = ProgressiveBatchConfig {
        session_id: session_id.clone(),
        sample_rate: file_sample_rate,
        segment_duration_ms,
        overlap_ms: 1000,
        max_concurrency: 2,
        base_url: params.base_url,
        api_key: params.api_key,
        model: params.model,
        language,
        provider: params.provider,
        session_dir,
    };

    let mut manager = ProgressiveBatchManager::new(config).with_runtime(runtime);

    // Stream PCM in chunks to avoid loading entire file into memory.
    const CHUNK_FRAMES: usize = 48000 * 10;
    let mut buf: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * file_channels);

    for sample in source {
        buf.push(sample);
        if buf.len() >= CHUNK_FRAMES * file_channels {
            let mono: Vec<f32> = if file_channels > 1 {
                buf.chunks_exact(file_channels)
                    .map(|c| c.iter().sum::<f32>() / file_channels as f32)
                    .collect()
            } else {
                std::mem::take(&mut buf)
            };
            manager.on_audio_frame(&mono);
            buf.clear();
        }
    }

    if !buf.is_empty() {
        let mono: Vec<f32> = if file_channels > 1 {
            buf.chunks_exact(file_channels)
                .map(|c| c.iter().sum::<f32>() / file_channels as f32)
                .collect()
        } else {
            buf
        };
        manager.on_audio_frame(&mono);
    }

    let response = manager
        .finish()
        .await
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed { message: e })?;

    Ok(BatchRunOutput {
        session_id,
        mode: BatchRunMode::Direct,
        response,
    })
}

async fn submit_file_direct(
    runtime: Arc<dyn BatchRuntime>,
    file_path: &std::path::Path,
    params: &BatchParams,
    session_id: &str,
) -> crate::Result<BatchRunOutput> {
    let file_bytes = tokio::fs::read(file_path).await.map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to read audio file: {e}"),
        }
    })?;

    let extension = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav");
    let mime_type = mime_type_for_extension(file_path);
    let file_name = format!("audio.{extension}");

    let part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str(mime_type)
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to create multipart: {e}"),
        })?;

    let listen_params = owhisper_interface::ListenParams {
        model: params.model.clone(),
        languages: params.languages.clone(),
        keywords: params.keywords.clone(),
        ..Default::default()
    };

    let url = owhisper_client::OpenAIAdapter::transcription_url(&params.base_url).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to build transcription URL: {e}"),
        }
    })?;

    let form =
        owhisper_client::OpenAIAdapter::build_batch_multipart(part, &listen_params, true, false)
            .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("failed to build request: {e}"),
            })?;

    let client = owhisper_client::create_client();
    let resp = client
        .post(url)
        .multipart(form)
        .header("Authorization", format!("Bearer {}", params.api_key))
        .send()
        .await
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("HTTP request failed: {e}"),
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("API returned {status}: {body_text}"),
        }
        .into());
    }

    let body = resp
        .text()
        .await
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to read response body: {e}"),
        })?;

    let response: owhisper_interface::batch::Response =
        if matches!(params.provider, crate::batch::BatchProvider::OpenAI) {
            owhisper_client::OpenAIAdapter::parse_batch_response(&body).map_err(|e| {
                crate::BatchFailure::ProgressiveBatchFailed {
                    message: format!("failed to parse response: {e}"),
                }
            })?
        } else {
            serde_json::from_str(&body).map_err(|e| {
                crate::BatchFailure::ProgressiveBatchFailed {
                    message: format!("failed to parse response: {e}"),
                }
            })?
        };

    runtime.emit(BatchEvent::BatchSegmentResult {
        session_id: session_id.to_string(),
        segment_index: 0,
        response: response.clone(),
    });

    Ok(BatchRunOutput {
        session_id: session_id.to_string(),
        mode: BatchRunMode::Direct,
        response,
    })
}
