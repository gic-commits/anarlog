mod submit;

use std::sync::Arc;
use std::time::Duration;

use hypr_audio_utils::{Source, source_from_path};
use hypr_transcribe_core::TARGET_SAMPLE_RATE;
use owhisper_interface::batch;

use hypr_pyannote_local::diarization::{DiarizationConfig, DiarizationManager};
use hypr_pyannote_local::duration_scheduler::{DurationSchedulerConfig, schedule_segments};

use submit::{DiarizationSubmitter, SpeakerSegmentData};

use crate::batch::{BatchParams, BatchRunMode, BatchRunOutput};
use crate::{BatchEvent, BatchRuntime};

pub(super) async fn run_diarization_batch(
    runtime: Arc<dyn BatchRuntime>,
    params: BatchParams,
) -> crate::Result<BatchRunOutput> {
    let session_id = params.session_id.clone();

    let audio = load_audio_to_mono_f32(&params.file_path)?;
    let sample_rate = TARGET_SAMPLE_RATE;

    runtime.emit(BatchEvent::DiarizationStarted {
        session_id: session_id.clone(),
        total_segments: 0,
    });

    let speaker_segments = run_diarization(&audio, sample_rate, &params)?;

    let groups = schedule_segments(
        speaker_segments,
        DurationSchedulerConfig {
            max_duration_ms: params.segment_duration_ms.unwrap_or(30000),
            ..Default::default()
        },
    );

    let total = groups.len();

    tracing::info!(
        "[diarization] {} groups to submit for session {}",
        total,
        session_id
    );

    let segment_data: Vec<SpeakerSegmentData> = groups
        .iter()
        .enumerate()
        .map(|(idx, group)| {
            let speaker = group
                .first()
                .map(|s: &hypr_pyannote_local::diarization::SpeakerSegment| s.speaker)
                .unwrap_or(0);
            let start_sample = (group.first().unwrap().start * sample_rate as f64) as usize;
            let end_sample = (group.last().unwrap().end * sample_rate as f64) as usize;
            let end_sample = end_sample.min(audio.len());
            SpeakerSegmentData {
                index: idx,
                speaker,
                global_start_ms: (group.first().unwrap().start * 1000.0) as i64,
                pcm_f32: audio[start_sample..end_sample].to_vec(),
            }
        })
        .collect();

    let results = run_submitter(params, runtime.clone(), session_id.clone(), segment_data).await?;

    let merged = merge_responses_by_speaker(results);

    runtime.emit(BatchEvent::BatchResponse {
        session_id: session_id.clone(),
        response: merged.clone(),
        mode: BatchRunMode::Direct,
    });
    runtime.emit(BatchEvent::BatchCompleted {
        session_id: session_id.clone(),
    });

    Ok(BatchRunOutput {
        session_id,
        mode: BatchRunMode::Direct,
        response: merged,
    })
}

async fn run_submitter(
    params: BatchParams,
    runtime: Arc<dyn BatchRuntime>,
    session_id: String,
    segment_data: Vec<SpeakerSegmentData>,
) -> crate::Result<Vec<(usize, batch::Response)>> {
    let total = segment_data.len();
    let mut submitter = DiarizationSubmitter::new(params.clone());
    submitter.enqueue_all(segment_data);

    let drain_timeout = Duration::from_secs(
        params.segment_duration_ms.unwrap_or(30000) as u64 * total.max(1) as u64 / 1000 + 60,
    );

    let raw_results = submitter.drain(drain_timeout).await;

    let (_pending, completed, failed) = submitter.progress();
    tracing::info!(
        "[diarization] session {}: {completed}/{total} completed, {failed} failed",
        session_id,
    );

    for (idx, speaker, response) in &raw_results {
        runtime.emit(BatchEvent::DiarizationSegmentResult {
            session_id: session_id.clone(),
            segment_index: *idx,
            speaker: *speaker,
            global_start_ms: 0,
            response: response.clone(),
        });
    }

    let results: Vec<(usize, batch::Response)> = raw_results
        .into_iter()
        .map(|(_, speaker, response)| (speaker, response))
        .collect();

    Ok(results)
}

fn load_audio_to_mono_f32(file_path: &str) -> crate::Result<Vec<f32>> {
    let source = source_from_path(std::path::Path::new(file_path)).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to open audio: {e}"),
        }
    })?;

    let src_sr: u32 = source.sample_rate().into();
    let channels: usize = source.channels().get().into();
    let samples: Vec<f32> = source.collect();

    let mono: Vec<f32> = if channels == 1 {
        samples
    } else {
        samples
            .chunks(channels)
            .map(|chunk| chunk.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    if src_sr == TARGET_SAMPLE_RATE {
        Ok(mono)
    } else {
        let ratio = src_sr as f64 / TARGET_SAMPLE_RATE as f64;
        let last = mono.len().saturating_sub(1);
        let out_len = (mono.len() as f64 / ratio).round() as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let pos = i as f64 * ratio;
            let lo = (pos.floor() as usize).min(last);
            let hi = (pos.ceil() as usize).min(last);
            let frac = pos - lo as f64;
            out.push(mono[lo] * (1.0 - frac as f32) + mono[hi] * frac as f32);
        }
        Ok(out)
    }
}

fn run_diarization(
    audio: &[f32],
    sample_rate: u32,
    params: &BatchParams,
) -> crate::Result<Vec<hypr_pyannote_local::diarization::SpeakerSegment>> {
    let audio_i16: Vec<i16> = audio
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();

    let config = DiarizationConfig {
        model_path: params.diarization_model.clone(),
        threshold: params.diarization_threshold,
        sample_rate,
        ..Default::default()
    };

    let mut manager = DiarizationManager::new(config).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to create DiarizationManager: {e}"),
        }
    })?;

    let result =
        manager
            .process(&audio_i16)
            .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("diarization failed: {e}"),
            })?;

    Ok(result.segments)
}

fn merge_responses_by_speaker(results: Vec<(usize, batch::Response)>) -> batch::Response {
    if results.is_empty() {
        return empty_response();
    }
    if results.len() == 1 {
        return results.into_iter().next().unwrap().1;
    }

    let mut all_words: Vec<batch::Word> = Vec::new();
    let mut all_text_parts: Vec<String> = Vec::new();
    let mut speakers: Vec<usize> = Vec::new();
    let mut responses: Vec<batch::Response> = Vec::new();

    for (speaker_idx, resp) in results {
        speakers.push(speaker_idx);
        responses.push(resp);
    }

    for (speaker, resp) in speakers.iter().zip(responses.iter()) {
        let alt = alt_from_response(resp);
        let text = alt.transcript;

        if !text.is_empty() {
            all_text_parts.push(format!("[Speaker_{speaker}] {text}"));
        }

        for mut word in alt.words.clone() {
            word.speaker = Some(*speaker);
            all_words.push(word);
        }
    }

    all_words.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    response_from_parts(all_text_parts.join("\n"), all_words)
}

fn alt_from_response(resp: &batch::Response) -> batch::Alternatives {
    resp.results
        .channels
        .first()
        .and_then(|c| c.alternatives.first())
        .cloned()
        .unwrap_or(batch::Alternatives {
            transcript: String::new(),
            confidence: 0.0,
            words: vec![],
        })
}

fn empty_response() -> batch::Response {
    serde_json::from_value(serde_json::json!({
        "metadata": {},
        "results": { "channels": [] }
    }))
    .expect("static json valid")
}

fn response_from_parts(text: String, words: Vec<batch::Word>) -> batch::Response {
    let avg_conf: f64 = if words.is_empty() {
        0.0
    } else {
        words.iter().map(|w| w.confidence).sum::<f64>() / words.len() as f64
    };

    serde_json::from_value(serde_json::json!({
        "metadata": {},
        "results": {
            "channels": [{
                "alternatives": [{
                    "transcript": text,
                    "confidence": avg_conf,
                    "words": words
                }]
            }]
        }
    }))
    .expect("response json valid")
}
