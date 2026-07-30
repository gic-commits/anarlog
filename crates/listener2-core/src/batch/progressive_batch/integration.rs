use std::sync::Arc;

use hypr_audio_utils::Source;
use hypr_pyannote_local::min_cut_merge::{MinCutMergeConfig, min_cut_merge};
use hypr_pyannote_local::segmentation::Segmenter;

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

const TARGET_SAMPLE_RATE: u32 = 16000;

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

    let src_sample_rate: u32 = source.sample_rate().into();
    let channels: usize = source.channels().get().into();
    let segment_duration_ms = params.segment_duration_ms.unwrap_or(30000);

    let duration_secs = source
        .total_duration()
        .map(|d| d.as_secs_f64())
        .unwrap_or(f64::MAX);

    if duration_secs < segment_duration_ms as f64 / 1000.0 {
        return submit_file_direct(
            runtime,
            std::path::Path::new(&file_path),
            &params,
            &session_id,
        )
        .await;
    }

    // Load full audio to mono f32 at source rate.
    let raw: Vec<f32> = source.collect();
    let mono_src: Vec<f32> = if channels > 1 {
        raw.chunks_exact(channels)
            .map(|c| c.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        raw
    };

    // Resample to TARGET_SAMPLE_RATE (16kHz) for VAD and transcription.
    let pcm_f32 = if src_sample_rate == TARGET_SAMPLE_RATE {
        mono_src
    } else {
        let ratio = src_sample_rate as f64 / TARGET_SAMPLE_RATE as f64;
        let last = mono_src.len().saturating_sub(1);
        let out_len = (mono_src.len() as f64 / ratio).round() as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let pos = i as f64 * ratio;
            let lo = (pos.floor() as usize).min(last);
            let hi = (pos.ceil() as usize).min(last);
            let frac = pos - lo as f64;
            out.push(mono_src[lo] * (1.0 - frac as f32) + mono_src[hi] * frac as f32);
        }
        out
    };

    // Convert to i16 for VAD.
    let pcm_i16: Vec<i16> = pcm_f32
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();

    // VAD segmentation.
    let mut segmenter = Segmenter::new(TARGET_SAMPLE_RATE).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("VAD init failed: {e}"),
        }
    })?;
    let vad_segments = segmenter
        .process(&pcm_i16, TARGET_SAMPLE_RATE)
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("VAD failed: {e}"),
        })?;

    let vad_count = vad_segments.len();

    let groups = min_cut_merge(
        vad_segments,
        &pcm_i16,
        TARGET_SAMPLE_RATE,
        MinCutMergeConfig {
            max_duration_ms: segment_duration_ms,
        },
    );

    if groups.is_empty() {
        return submit_file_direct(
            runtime,
            std::path::Path::new(&file_path),
            &params,
            &session_id,
        )
        .await;
    }

    let total = groups.len();
    tracing::info!(
        "[vad-batch] {} groups from {} VAD segments for session {}",
        total,
        vad_count,
        session_id,
    );

    let session_dir = std::env::temp_dir().join(format!("progressive-batch-{session_id}"));
    let _ = std::fs::create_dir_all(&session_dir);

    let stitcher_config = StitcherConfig {
        overlap_ms: 0,
        segment_duration_ms: segment_duration_ms as u64,
        total_segments: total,
    };
    let mut stitcher = Stitcher::new(stitcher_config);

    for (group_idx, group) in groups.iter().enumerate() {
        let start_sample = (group[0].start * TARGET_SAMPLE_RATE as f64) as usize;
        let end_sample = ((group.last().unwrap().end * TARGET_SAMPLE_RATE as f64) as usize)
            .min(pcm_i16.len());
        let global_start_ms = (group[0].start * 1000.0) as i64;

        let audio_bytes = pcm_i16[start_sample..end_sample]
            .iter()
            .flat_map(|&s| s.to_le_bytes())
            .collect::<Vec<u8>>();

        let file_part = reqwest::multipart::Part::bytes(audio_bytes)
            .file_name("audio.raw")
            .mime_str("audio/pcm")
            .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("failed to create multipart: {e}"),
            })?;

        let listen_params = owhisper_interface::ListenParams {
            model: params.model.clone(),
            languages: params.languages.clone(),
            ..Default::default()
        };

        let url = owhisper_client::OpenAIAdapter::transcription_url(&params.base_url)
            .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("invalid URL: {e}"),
            })?;

        let form =
            owhisper_client::OpenAIAdapter::build_batch_multipart(file_part, &listen_params, true, false)
                .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                    message: format!("failed to build form: {e}"),
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
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "[vad-batch] group {} failed ({}): {}",
                group_idx,
                status,
                body,
            );
            stitcher.add_abandoned(group_idx);
            continue;
        }

        let body = resp
            .text()
            .await
            .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("failed to read response body: {e}"),
            })?;

        let segment_response: owhisper_interface::batch::Response =
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
            session_id: session_id.clone(),
            segment_index: group_idx,
            global_start_ms,
            response: segment_response.clone(),
        });

        stitcher.add_segment(CompletedSegment {
            index: group_idx,
            global_start_ms,
            response: segment_response,
        });
    }

    let has_completed = stitcher.segment_count() > 0;
    if !has_completed {
        return Err(crate::BatchFailure::ProgressiveBatchFailed {
            message: "no groups completed successfully".to_string(),
        }
        .into());
    }

    match stitcher.stitch() {
        Ok(response) => {
            let _ = std::fs::remove_dir_all(&session_dir);
            Ok(BatchRunOutput {
                session_id,
                mode: BatchRunMode::Direct,
                response,
            })
        }
        Err(e) => Err(crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("stitch failed: {e:?}"),
        }
        .into()),
    }
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
        global_start_ms: 0,
        response: response.clone(),
    });

    Ok(BatchRunOutput {
        session_id: session_id.to_string(),
        mode: BatchRunMode::Direct,
        response,
    })
}

/// Continue a previously interrupted/partial progressive batch.
/// This still uses the old fixed-segment ProgressiveBatchManager for
/// backward compatibility with existing DB records.
pub async fn continue_from_file(
    runtime: Arc<dyn BatchRuntime>,
    params: BatchParams,
    completed_segments: Vec<PersistedCompletedSegment>,
) -> crate::Result<BatchRunOutput> {
    let session_id = params.session_id.clone();
    let file_path = params.file_path.clone();

    let source = hypr_audio_utils::source_from_path(&file_path).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to open audio file: {e}"),
        }
    })?;

    let sample_rate: u32 = source.sample_rate().into();
    let channels: usize = source.channels().get().into();
    let segment_duration_ms = params.segment_duration_ms.unwrap_or(30000);
    let session_dir = std::env::temp_dir().join(format!("progressive-batch-{session_id}"));
    let _ = std::fs::create_dir_all(&session_dir);

    let language = params.languages.first().map(|l| l.to_string());

    let config = ProgressiveBatchConfig {
        session_id: session_id.clone(),
        sample_rate,
        segment_duration_ms,
        overlap_ms: params.overlap_ms.unwrap_or(1000),
        max_concurrency: params.max_concurrency.unwrap_or(2) as usize,
        base_url: params.base_url,
        api_key: params.api_key,
        model: params.model,
        language,
        provider: params.provider,
        session_dir,
    };

    let mut manager =
        ProgressiveBatchManager::resume(config, completed_segments).with_runtime(runtime);

    const CHUNK_FRAMES: usize = 48000 * 10;
    let mut buf: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * channels);

    for sample in source {
        buf.push(sample);
        if buf.len() >= CHUNK_FRAMES * channels {
            let mono: Vec<f32> = if channels > 1 {
                buf.chunks_exact(channels)
                    .map(|c| c.iter().sum::<f32>() / channels as f32)
                    .collect()
            } else {
                std::mem::take(&mut buf)
            };
            manager.on_audio_frame(&mono);
            buf.clear();
        }
    }

    if !buf.is_empty() {
        let mono: Vec<f32> = if channels > 1 {
            buf.chunks_exact(channels)
                .map(|c| c.iter().sum::<f32>() / channels as f32)
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
