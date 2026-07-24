use hypr_audio_utils::Source;

use crate::batch::{BatchParams, BatchRunOutput, BatchRunMode};

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
    params: BatchParams,
    _sample_rate: u32,
    _channels: u8,
) -> crate::Result<BatchRunOutput> {
    let session_id = params.session_id.clone();
    let file_path = params.file_path.clone();

    let source = hypr_audio_utils::source_from_path(&file_path)
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to open audio file: {e}"),
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
        return submit_file_direct(std::path::Path::new(&file_path), &params, &session_id).await;
    }

    let session_dir = std::env::temp_dir().join(format!("progressive-batch-{session_id}"));
    let _ = std::fs::create_dir_all(&session_dir);

    let language = params.languages.first().map(|l| l.to_string());

    let config = ProgressiveBatchConfig {
        sample_rate: file_sample_rate,
        segment_duration_ms,
        overlap_ms: 1000,
        max_concurrency: 2,
        min_duration_secs: 0,
        base_url: params.base_url,
        api_key: params.api_key,
        model: params.model,
        language,
        provider: params.provider,
        session_dir,
    };

    let mut manager = ProgressiveBatchManager::new(config);

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
    file_path: &std::path::Path,
    params: &BatchParams,
    session_id: &str,
) -> crate::Result<BatchRunOutput> {
    let file_bytes = tokio::fs::read(file_path)
        .await
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to read audio file: {e}"),
        })?;

    let extension = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav");
    let mime_type = mime_type_for_extension(file_path);
    let file_name = format!("audio.{extension}");

    let client = reqwest::Client::new();
    let mut url: url::Url = params
        .base_url
        .parse()
        .map_err(|e: url::ParseError| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("invalid base_url: {e}"),
        })?;
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("audio/transcriptions") {
        url.path_segments_mut()
            .expect("base_url is a valid URL with segments")
            .pop_if_empty()
            .push("audio/transcriptions");
    }

    let part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str(mime_type)
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to create multipart: {e}"),
        })?;

    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", params.model.clone().unwrap_or_default())
        .text("response_format", "json");

    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", params.api_key))
        .multipart(form)
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

    let response: owhisper_interface::batch::Response = if matches!(
        params.provider,
        crate::batch::BatchProvider::OpenAI
    ) {
        owhisper_client::OpenAIAdapter::parse_batch_response(&body)
            .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("failed to parse response: {e}"),
            })?
    } else {
        serde_json::from_str(&body)
            .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("failed to parse response: {e}"),
            })?
    };

    Ok(BatchRunOutput {
        session_id: session_id.to_string(),
        mode: BatchRunMode::Direct,
        response,
    })
}
