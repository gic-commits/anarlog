use std::sync::Arc;

use hypr_audio_utils::Source;
use hypr_pyannote_local::incremental_diarization::{
    IncrementalDiarizationConfig, IncrementalDiarizationEngine,
};

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

    let audio_duration_ms = source
        .total_duration()
        .map(|d| (d.as_secs_f64() * 1000.0) as u64);

    tracing::info!(
        "[vad-batch] run_progressive_batch_from_file: session={} segment_duration_ms={} diarization_enabled={} diarization_model={:?} diarization_threshold={}",
        session_id,
        segment_duration_ms,
        params.diarization_enabled,
        params.diarization_model,
        params.diarization_threshold,
    );

    let session_dir = std::env::temp_dir().join(format!("progressive-batch-{session_id}"));
    let _ = std::fs::create_dir_all(&session_dir);

    let language = params.languages.first().map(|l| l.to_string());

    // Stream through the same ProgressiveBatchManager the live path uses:
    // VadGroupStream segments on the fly, each group is submitted as soon as
    // it emits (streaming results back), and the whole pipeline stays live —
    // no long event-free preparation phase to trip idle timeouts regardless of
    // audio length or segment duration.
    let config = ProgressiveBatchConfig {
        session_id: session_id.clone(),
        sample_rate: TARGET_SAMPLE_RATE,
        segment_duration_ms,
        overlap_ms: 0,
        max_concurrency: params.max_concurrency.unwrap_or(2) as usize,
        base_url: params.base_url,
        api_key: params.api_key,
        model: params.model,
        language,
        provider: params.provider,
        session_dir,
        vad_groups: true,
        collect_vad_segments: params.diarization_enabled,
        audio_duration_ms,
    };
    let mut manager = ProgressiveBatchManager::new(config).with_runtime(runtime);

    // Diarization (optional): VAD segments drained from the stream feed the
    // engine as they're produced. Embedding/clustering are comparatively slow
    // but the audio stream keeps producing groups + segmentResult events, so
    // the idle monitor is never starved.
    let mut diarization_engine: Option<IncrementalDiarizationEngine> = if params.diarization_enabled
    {
        match IncrementalDiarizationEngine::new(IncrementalDiarizationConfig {
            sample_rate: TARGET_SAMPLE_RATE,
            model_path: params.diarization_model.clone(),
            threshold: params.diarization_threshold,
            recluster_interval: usize::MAX,
            ..Default::default()
        }) {
            Ok(engine) => Some(engine),
            Err(e) => {
                tracing::warn!(error = %e, "[vad-batch] diarization init failed, continuing without");
                None
            }
        }
    } else {
        None
    };

    // Stream-decode and feed in 10s chunks (mirrors continue_from_file).
    const CHUNK_FRAMES: usize = 48000 * 10;
    let mut buf: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * channels);
    let mut resampler = StreamingResampler::new(src_sample_rate, TARGET_SAMPLE_RATE);
    let mut src_index = 0usize;

    let mut drain_diarization =
        |manager: &mut ProgressiveBatchManager,
         engine: &mut Option<IncrementalDiarizationEngine>| {
            if let Some(eng) = engine.as_mut() {
                let vad_segments = manager.take_vad_segments();
                if !vad_segments.is_empty() {
                    tracing::info!(
                        "[vad-batch] diarization feed {} vad segments",
                        vad_segments.len()
                    );
                    if let Err(e) = eng.feed_segments(&vad_segments) {
                        tracing::warn!(error = %e, "[vad-batch] diarization feed error");
                    }
                }
            }
        };

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
            let mut resampled = Vec::new();
            resampler.push(&mono, src_index, &mut resampled);
            src_index += mono.len();
            manager.on_audio_frame(&resampled);
            drain_diarization(&mut manager, &mut diarization_engine);
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
        let mut resampled = Vec::new();
        resampler.push(&mono, src_index, &mut resampled);
        manager.on_audio_frame(&resampled);
        drain_diarization(&mut manager, &mut diarization_engine);
    }

    let mut response = manager
        .finish()
        .await
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed { message: e })?;

    // Final diarization pass: feed any tail VAD segments flushed during
    // finish, finalize clustering, then label words by their mid time.
    if let Some(ref mut eng) = diarization_engine {
        let tail = manager.take_vad_segments();
        if !tail.is_empty() {
            tracing::info!(
                "[vad-batch] diarization tail feed {} vad segments",
                tail.len()
            );
            if let Err(e) = eng.feed_segments(&tail) {
                tracing::warn!(error = %e, "[vad-batch] diarization tail feed error");
            }
        }
        let t0 = std::time::Instant::now();
        eng.finalize();
        let speaker_segs = eng.speaker_segments();
        let unique: std::collections::HashSet<usize> =
            speaker_segs.iter().map(|s| s.speaker).collect();
        tracing::info!(
            "[vad-batch] diarization finalize: {} speaker segments, {} unique speakers, took {:.1}s",
            speaker_segs.len(),
            unique.len(),
            t0.elapsed().as_secs_f64()
        );
        let mut labeled = 0usize;
        for channel in &mut response.results.channels {
            for alt in &mut channel.alternatives {
                for word in &mut alt.words {
                    let mid = (word.start + word.end) / 2.0;
                    word.speaker = eng.speaker_at_time(mid);
                    labeled += 1;
                }
            }
        }
        let labeled_unique: std::collections::HashSet<Option<usize>> = response
            .results
            .channels
            .iter()
            .flat_map(|c| &c.alternatives)
            .flat_map(|a| &a.words)
            .map(|w| w.speaker)
            .collect();
        tracing::info!(
            "[vad-batch] diarization labeled {labeled} words, unique speaker values: {:?}",
            labeled_unique.iter().flatten().collect::<Vec<_>>()
        );
        propagate_speaker_to_none(&mut response);
    }

    let word_count = response
        .results
        .channels
        .first()
        .and_then(|c| c.alternatives.first())
        .map(|a| a.words.len())
        .unwrap_or(0);
    tracing::info!(
        "[vad-batch] stitch ok  words={}  last_end={:.1}s",
        word_count,
        response
            .results
            .channels
            .first()
            .and_then(|c| c.alternatives.first())
            .and_then(|a| a.words.last())
            .map(|w| w.end)
            .unwrap_or(0.0),
    );

    let _ = std::fs::remove_dir_all(
        &std::env::temp_dir().join(format!("progressive-batch-{session_id}")),
    );
    Ok(BatchRunOutput {
        session_id,
        mode: BatchRunMode::Direct,
        response,
    })
}

/// Propagate a valid speaker value to words with `speaker: None`.
/// Uses nearest valid speaker, scanning forward then backward.
pub fn propagate_speaker_to_none(response: &mut owhisper_interface::batch::Response) {
    for channel in &mut response.results.channels {
        for alt in &mut channel.alternatives {
            let n = alt.words.len();
            if n == 0 {
                continue;
            }

            // Forward fill
            let mut last: Option<usize> = None;
            for w in alt.words.iter_mut() {
                if let Some(s) = w.speaker {
                    last = Some(s);
                } else if let Some(s) = last {
                    w.speaker = Some(s);
                }
            }

            // Backward fill (catches leading None words)
            let mut last: Option<usize> = None;
            for w in alt.words.iter_mut().rev() {
                if let Some(s) = w.speaker {
                    last = Some(s);
                } else if let Some(s) = last {
                    w.speaker = Some(s);
                }
            }
        }
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

    let response: owhisper_interface::batch::Response = if matches!(
        params.provider,
        crate::batch::BatchProvider::OpenAI | crate::batch::BatchProvider::Groq
    ) {
        owhisper_client::OpenAIAdapter::parse_batch_response(&body).map_err(|e| {
            crate::BatchFailure::ProgressiveBatchFailed {
                message: format!("failed to parse response: {e}"),
            }
        })?
    } else {
        serde_json::from_str(&body).map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to parse response: {e}"),
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
///
/// The file re-transcribe path (`run_progressive_batch_from_file`) always
/// groups by streaming VAD + Min-Cut/Merge at 16 kHz, so Continue replays the
/// file through the same `VadGroupStream` (resampling to 16 kHz) to reproduce
/// identical group indices for the pre-loaded completed segments.
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
    let audio_duration_ms = source
        .total_duration()
        .map(|d| (d.as_secs_f64() * 1000.0) as u64);

    let config = ProgressiveBatchConfig {
        session_id: session_id.clone(),
        sample_rate: TARGET_SAMPLE_RATE,
        segment_duration_ms,
        overlap_ms: 0,
        max_concurrency: params.max_concurrency.unwrap_or(2) as usize,
        base_url: params.base_url,
        api_key: params.api_key,
        model: params.model,
        language,
        provider: params.provider,
        session_dir,
        vad_groups: true,
        collect_vad_segments: false,
        audio_duration_ms,
    };

    let mut manager =
        ProgressiveBatchManager::resume(config, completed_segments).with_runtime(runtime);

    const CHUNK_FRAMES: usize = 48000 * 10;
    let mut buf: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * channels);
    let mut resampler = StreamingResampler::new(sample_rate, TARGET_SAMPLE_RATE);
    let mut src_index = 0usize;

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
            let mut resampled = Vec::new();
            resampler.push(&mono, src_index, &mut resampled);
            src_index += mono.len();
            manager.on_audio_frame(&resampled);
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
        let mut resampled = Vec::new();
        resampler.push(&mono, src_index, &mut resampled);
        manager.on_audio_frame(&resampled);
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

/// Streaming linear-interpolation resampler matching the whole-file resampler
/// in `run_progressive_batch_from_file`: output sample *i* is interpolated at
/// source position `i * ratio`, with `pos` advanced `ratio` per output.
struct StreamingResampler {
    ratio: f64,
    pos: f64,
}

impl StreamingResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            ratio: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
        }
    }

    fn push(&mut self, input: &[f32], chunk_start: usize, out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let src_start = chunk_start as f64;
        let src_end = (chunk_start + input.len()) as f64;
        let last = input.len() - 1;
        while self.pos < src_end {
            let f = (self.pos - src_start).max(0.0);
            let lo = f.floor() as usize;
            let hi = (lo + 1).min(last);
            let frac = f - lo as f64;
            out.push(if lo < input.len() {
                input[lo] * (1.0 - frac as f32) + input[hi] * frac as f32
            } else {
                input[last]
            });
            self.pos += self.ratio;
        }
    }
}

/// Compute the full-archive VAD group sequence for an audio file, the same
/// grouping the live `VadGroupStream` would produce if fed the whole file.
/// Returns `(group_index, global_start_ms)` pairs, 0-based and ordered.
///
/// Used to re-align live-completed segments (whose indices came from a
/// frame-dropped stream) onto the full-audio group indices so a subsequent
/// `continue_from_file` can correctly skip already-transcribed groups.
pub fn compute_full_audio_groups(
    audio_path: &std::path::Path,
    segment_duration_ms: u32,
) -> Result<Vec<(usize, i64)>, crate::Error> {
    let source = hypr_audio_utils::source_from_path(audio_path).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to open audio file: {e}"),
        }
    })?;
    let src_sample_rate: u32 = source.sample_rate().into();
    let channels: usize = source.channels().get().into();
    let raw: Vec<f32> = source.collect();
    let mono: Vec<f32> = if channels > 1 {
        raw.chunks_exact(channels)
            .map(|c| c.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        raw
    };
    let pcm = resample_to_16k(&mono, src_sample_rate);
    let i16: Vec<i16> = pcm
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();

    let mut stream = VadGroupStream::new(VadGroupStreamConfig {
        sample_rate: TARGET_SAMPLE_RATE,
        max_duration_ms: segment_duration_ms,
        collect_vad_segments: false,
    })
    .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed { message: e })?;

    const CHUNK: usize = 16000 / 10;
    let mut out = Vec::new();
    for chunk in i16.chunks(CHUNK) {
        let f32s: Vec<f32> = chunk.iter().map(|&s| s as f32 / 32767.0).collect();
        out.extend(stream.feed(&f32s));
    }
    out.extend(stream.flush());

    let mut groups = out
        .into_iter()
        .map(|seg| (seg.index, seg.global_start_ms))
        .collect::<Vec<_>>();
    groups.sort_by_key(|(i, _)| *i);
    Ok(groups)
}

/// Map live-completed segments (whose `global_start_ms`/`index` come from a
/// possibly frame-dropped live stream) onto the full-audio group indices.
/// Each live segment is matched to the full-audio group whose start time is
/// closest; duplicate matches collapse to the first.
pub fn align_completed_segments_to_full_audio(
    completed: &[PersistedCompletedSegment],
    full_groups: &[(usize, i64)],
) -> Vec<PersistedCompletedSegment> {
    if full_groups.is_empty() {
        return completed.to_vec();
    }
    let mut aligned = Vec::with_capacity(completed.len());
    let mut used = std::collections::HashSet::new();
    for seg in completed {
        // Binary search the group whose start_ms is nearest to seg.global_start_ms.
        let mut lo = 0usize;
        let mut hi = full_groups.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if full_groups[mid].1 < seg.global_start_ms {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let best = if lo == 0 {
            0
        } else if lo >= full_groups.len() {
            full_groups.len() - 1
        } else {
            let prev = full_groups[lo - 1].1;
            let next = full_groups[lo].1;
            if (seg.global_start_ms - prev).abs() <= (next - seg.global_start_ms).abs() {
                lo - 1
            } else {
                lo
            }
        };
        let (gi, _) = full_groups[best];
        if used.insert(gi) {
            aligned.push(PersistedCompletedSegment {
                index: gi,
                global_start_ms: seg.global_start_ms,
                response: seg.response.clone(),
            });
        }
    }
    aligned
}

fn resample_to_16k(mono: &[f32], src_rate: u32) -> Vec<f32> {
    if src_rate == TARGET_SAMPLE_RATE {
        return mono.to_vec();
    }
    let ratio = src_rate as f64 / TARGET_SAMPLE_RATE as f64;
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(index: usize, start_ms: i64) -> PersistedCompletedSegment {
        PersistedCompletedSegment {
            index,
            global_start_ms: start_ms,
            response: owhisper_interface::batch::Response {
                metadata: serde_json::json!({}),
                results: owhisper_interface::batch::Results { channels: vec![] },
            },
        }
    }

    #[test]
    fn align_maps_live_segments_to_nearest_full_group() {
        // Full audio groups at 10s, 40s, 70s (simulating a dropped-stream offset).
        let full = vec![(0usize, 10_000i64), (1, 40_000), (2, 70_000)];
        // Live completed segments were offset: started ~3s early due to drops.
        let live = vec![
            completed(0, 7_000),
            completed(1, 37_000),
            completed(2, 67_000),
        ];
        let aligned = align_completed_segments_to_full_audio(&live, &full);
        assert_eq!(aligned.len(), 3);
        assert_eq!(aligned[0].index, 0);
        assert_eq!(aligned[1].index, 1);
        assert_eq!(aligned[2].index, 2);
    }

    #[test]
    fn align_collapses_duplicate_matches() {
        let full = vec![(0usize, 10_000i64), (1, 40_000)];
        // Two live segments both nearest to group 0.
        let live = vec![completed(0, 11_000), completed(1, 12_000)];
        let aligned = align_completed_segments_to_full_audio(&live, &full);
        assert_eq!(aligned.len(), 1);
        assert_eq!(aligned[0].index, 0);
    }
}

#[cfg(test)]
mod real_audio_tests {
    use super::*;

    /// End-to-end check on a real recording: full-audio groups must align back
    /// to themselves (identity), and a "dropped" subset must map onto distinct
    /// full groups with correct ordering. Requires ANARLOG_TEST_AUDIO.
    #[test]
    fn full_groups_align_identity_on_real_audio() {
        let Some(path) = std::env::var("ANARLOG_TEST_AUDIO").ok() else {
            eprintln!("skipping: ANARLOG_TEST_AUDIO not set");
            return;
        };
        let groups = compute_full_audio_groups(std::path::Path::new(&path), 30000).unwrap();
        assert!(!groups.is_empty(), "expected groups for real audio");
        eprintln!("[align] full-audio groups: {}", groups.len());

        // Identity: completed segments built from the groups themselves must
        // map back to the same indices.
        let live: Vec<PersistedCompletedSegment> = groups
            .iter()
            .map(|(i, start_ms)| completed_for_real(*i, *start_ms))
            .collect();
        let aligned = align_completed_segments_to_full_audio(&live, &groups);
        assert_eq!(aligned.len(), groups.len());
        for (a, g) in aligned.iter().zip(groups.iter()) {
            assert_eq!(a.index, g.0, "identity alignment must preserve index");
        }

        // A dropped subset (every 3rd group) still maps to distinct groups.
        let dropped: Vec<PersistedCompletedSegment> = groups
            .iter()
            .step_by(3)
            .map(|(i, start_ms)| completed_for_real(*i, *start_ms))
            .collect();
        let aligned2 = align_completed_segments_to_full_audio(&dropped, &groups);
        let indices: std::collections::HashSet<usize> = aligned2.iter().map(|s| s.index).collect();
        assert_eq!(
            indices.len(),
            aligned2.len(),
            "no duplicate matches expected"
        );
        eprintln!(
            "[align] dropped-subset aligned to {} distinct groups",
            indices.len()
        );
    }

    fn completed_for_real(index: usize, start_ms: i64) -> PersistedCompletedSegment {
        PersistedCompletedSegment {
            index,
            global_start_ms: start_ms,
            response: owhisper_interface::batch::Response {
                metadata: serde_json::json!({}),
                results: owhisper_interface::batch::Results { channels: vec![] },
            },
        }
    }
}
