use std::sync::Arc;
use std::time::{Duration, Instant};

use owhisper_client::OpenAIAdapter;
use owhisper_interface::batch;
use tokio::sync::mpsc;
use tracing::Instrument;

use hypr_transcribe_core::TARGET_SAMPLE_RATE;

use crate::batch::BatchParams;

const MAX_RETRIES: u32 = 3;
const MAX_CONCURRENCY: usize = 2;

#[derive(Clone)]
pub struct SpeakerSegmentData {
    pub index: usize,
    pub speaker: usize,
    pub global_start_ms: i64,
    pub pcm_f32: Vec<f32>,
}

impl Default for SpeakerSegmentData {
    fn default() -> Self {
        Self {
            index: 0,
            speaker: 0,
            global_start_ms: 0,
            pcm_f32: Vec::new(),
        }
    }
}

enum SegmentStatus {
    Pending(SpeakerSegmentData),
    InFlight {
        data: SpeakerSegmentData,
        started_at: Instant,
    },
    Completed {
        data: SpeakerSegmentData,
        response: batch::Response,
    },
    Failed {
        data: SpeakerSegmentData,
        error: String,
        retry_count: u32,
    },
}

impl SegmentStatus {
    fn data(&self) -> &SpeakerSegmentData {
        match self {
            SegmentStatus::Pending(d)
            | SegmentStatus::InFlight { data: d, .. }
            | SegmentStatus::Completed { data: d, .. }
            | SegmentStatus::Failed { data: d, .. } => d,
        }
    }

    fn take_data(&mut self) -> SpeakerSegmentData {
        match self {
            SegmentStatus::Pending(d) => std::mem::take(d),
            SegmentStatus::InFlight { data, .. } => std::mem::take(data),
            SegmentStatus::Completed { data, .. } => std::mem::take(data),
            SegmentStatus::Failed { data, .. } => std::mem::take(data),
        }
    }
}

enum WorkerResult {
    Completed {
        index: usize,
        response: batch::Response,
    },
    Failed {
        index: usize,
        error: String,
        exhausted: bool,
    },
}

pub struct DiarizationSubmitter {
    segments: Vec<SegmentStatus>,
    inflight_count: usize,
    result_rx: mpsc::UnboundedReceiver<WorkerResult>,
    result_tx: mpsc::UnboundedSender<WorkerResult>,
    params: Arc<BatchParams>,
    cancelled: bool,
}

impl DiarizationSubmitter {
    pub fn new(params: BatchParams) -> Self {
        let (result_tx, result_rx) = mpsc::unbounded_channel();
        Self {
            segments: Vec::new(),
            inflight_count: 0,
            result_rx,
            result_tx,
            params: Arc::new(params),
            cancelled: false,
        }
    }

    pub fn enqueue(&mut self, data: SpeakerSegmentData) {
        self.segments.push(SegmentStatus::Pending(data));
        self.try_dispatch();
    }

    pub fn enqueue_all(&mut self, segments: Vec<SpeakerSegmentData>) {
        for seg in segments {
            self.segments.push(SegmentStatus::Pending(seg));
        }
        self.try_dispatch();
    }

    pub fn poll_completed(&mut self) -> Vec<(usize, usize, batch::Response)> {
        while let Ok(worker_result) = self.result_rx.try_recv() {
            self.apply_result(worker_result);
        }
        self.collect_completed()
    }

    pub fn progress(&self) -> (usize, usize, usize) {
        let mut pending = 0;
        let mut completed = 0;
        let mut failed = 0;
        for seg in &self.segments {
            match seg {
                SegmentStatus::Pending(_) => pending += 1,
                SegmentStatus::InFlight { .. } => {}
                SegmentStatus::Completed { .. } => completed += 1,
                SegmentStatus::Failed { .. } => failed += 1,
            }
        }
        (pending, completed, failed)
    }

    pub fn total(&self) -> usize {
        self.segments.len()
    }

    pub async fn drain(&mut self, timeout: Duration) -> Vec<(usize, usize, batch::Response)> {
        let deadline = Instant::now() + timeout;

        loop {
            self.try_dispatch();
            let remaining = self.remaining_inflight_or_pending();
            if remaining == 0 {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!(
                    "[diarization::drain] timeout reached, {} remaining",
                    remaining
                );
                self.fail_remaining("drain timeout");
                break;
            }
            let wait = deadline - now;
            tokio::select! {
                result = self.result_rx.recv() => {
                    match result {
                        Some(worker_result) => self.apply_result(worker_result),
                        None => break,
                    }
                }
                _ = tokio::time::sleep(wait) => {
                    self.fail_remaining("drain timeout");
                    break;
                }
            }
        }

        self.collect_completed()
    }

    fn remaining_inflight_or_pending(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    SegmentStatus::Pending(_) | SegmentStatus::InFlight { .. }
                )
            })
            .count()
    }

    fn fail_remaining(&mut self, reason: &str) {
        for seg in &mut self.segments {
            if matches!(
                seg,
                SegmentStatus::Pending(_) | SegmentStatus::InFlight { .. }
            ) {
                let data = seg.take_data();
                *seg = SegmentStatus::Failed {
                    data,
                    error: reason.to_string(),
                    retry_count: 0,
                };
            }
        }
    }

    fn collect_completed(&self) -> Vec<(usize, usize, batch::Response)> {
        let mut results: Vec<(usize, usize, batch::Response)> = Vec::new();
        for seg in &self.segments {
            if let SegmentStatus::Completed { data, response } = seg {
                results.push((data.index, data.speaker, response.clone()));
            }
        }
        results.sort_by_key(|(idx, _, _)| *idx);
        results
    }

    fn try_dispatch(&mut self) {
        if self.cancelled {
            return;
        }
        while self.inflight_count < MAX_CONCURRENCY {
            let pos = self
                .segments
                .iter()
                .position(|s| matches!(s, SegmentStatus::Pending(_)));
            match pos {
                Some(idx) => {
                    let data = match &self.segments[idx] {
                        SegmentStatus::Pending(d) => d.clone(),
                        _ => continue,
                    };
                    let params = self.params.clone();
                    let tx = self.result_tx.clone();

                    self.segments[idx] = SegmentStatus::InFlight {
                        data: data.clone(),
                        started_at: Instant::now(),
                    };
                    self.inflight_count += 1;

                    let span = tracing::info_span!("diarization_submit", idx = data.index);
                    tokio::spawn(
                        async move {
                            let result =
                                submit_with_retry(&params, &data.pcm_f32, data.index).await;
                            let _ = tx.send(result);
                        }
                        .instrument(span),
                    );
                }
                None => break,
            }
        }
    }

    fn apply_result(&mut self, worker_result: WorkerResult) {
        self.inflight_count = self.inflight_count.saturating_sub(1);
        match worker_result {
            WorkerResult::Completed { index, response } => {
                if let Some(seg) = self.segments.iter_mut().find(|s| s.data().index == index) {
                    let data = seg.take_data();
                    *seg = SegmentStatus::Completed { data, response };
                }
            }
            WorkerResult::Failed {
                index,
                error,
                exhausted,
            } => {
                let retry_count = self.count_retries(index);
                if let Some(seg) = self.segments.iter_mut().find(|s| s.data().index == index) {
                    let data = seg.take_data();
                    *seg = SegmentStatus::Failed {
                        data,
                        error: error.clone(),
                        retry_count: retry_count + 1,
                    };
                }
                if !exhausted && retry_count < MAX_RETRIES {
                    tracing::warn!(
                        "[diarization] retrying segment {} (attempt {}/{}): {}",
                        index,
                        retry_count + 1,
                        MAX_RETRIES,
                        error,
                    );
                    if let Some(seg) = self.segments.iter_mut().find(|s| s.data().index == index) {
                        let data = seg.take_data();
                        *seg = SegmentStatus::Pending(data);
                    }
                    self.try_dispatch();
                } else {
                    tracing::error!(
                        "[diarization] segment {} failed permanently after {} retries: {}",
                        index,
                        retry_count + 1,
                        error,
                    );
                }
            }
        }
    }

    fn count_retries(&self, index: usize) -> u32 {
        self.segments
            .iter()
            .filter(|s| {
                if let SegmentStatus::Failed { data, .. } = s {
                    data.index == index
                } else {
                    false
                }
            })
            .count() as u32
    }
}

async fn submit_with_retry(params: &BatchParams, pcm_f32: &[f32], index: usize) -> WorkerResult {
    let mut last_error = String::new();
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let delay = Duration::from_millis(500 * (1 << attempt));
            tokio::time::sleep(delay).await;
        }
        match submit_speaker_segment(pcm_f32, TARGET_SAMPLE_RATE, params).await {
            Ok(response) => {
                return WorkerResult::Completed { index, response };
            }
            Err(e) => {
                last_error = e.to_string();
                tracing::warn!(
                    "[diarization] submit attempt {}/{} for segment {} failed: {}",
                    attempt + 1,
                    MAX_RETRIES,
                    index,
                    last_error,
                );
            }
        }
    }
    WorkerResult::Failed {
        index,
        error: last_error,
        exhausted: true,
    }
}

async fn submit_speaker_segment(
    pcm_f32: &[f32],
    sample_rate: u32,
    params: &BatchParams,
) -> crate::Result<batch::Response> {
    let resampled = if sample_rate == TARGET_SAMPLE_RATE {
        pcm_f32.to_vec()
    } else {
        let ratio = sample_rate as f64 / TARGET_SAMPLE_RATE as f64;
        let last = pcm_f32.len().saturating_sub(1);
        let out_len = (pcm_f32.len() as f64 / ratio).round() as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let pos = i as f64 * ratio;
            let lo = (pos.floor() as usize).min(last);
            let hi = (pos.ceil() as usize).min(last);
            let frac = pos - lo as f64;
            out.push(pcm_f32[lo] * (1.0 - frac as f32) + pcm_f32[hi] * frac as f32);
        }
        out
    };

    let pcm_bytes: Vec<u8> = resampled
        .iter()
        .flat_map(|&s| {
            let clamped = s.clamp(-1.0, 1.0) * 32767.0;
            (clamped as i16).to_le_bytes()
        })
        .collect();

    let file_part = reqwest::multipart::Part::bytes(pcm_bytes)
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

    let url = OpenAIAdapter::transcription_url(&params.base_url).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("invalid URL: {e}"),
        }
    })?;

    let form = OpenAIAdapter::build_batch_multipart(file_part, &listen_params, true, false)
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

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to read response body: {e}"),
        })?;

    if !status.is_success() {
        return Err(crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("API error ({}): {}", status.as_u16(), body),
        }
        .into());
    }

    serde_json::from_str(&body).map_err(|e| {
        crate::BatchFailure::ProgressiveBatchFailed {
            message: format!("failed to parse response: {e} (body: {body})"),
        }
        .into()
    })
}
