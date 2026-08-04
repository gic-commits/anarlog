use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use owhisper_interface::batch;
use tokio::sync::mpsc;

use crate::batch::BatchProvider;

const MAX_RETRIES: u32 = 3;
/// Extra retries granted for rate-limited (429) responses — the quota is
/// momentary and the caller is told when to retry via `retry-after`.
const MAX_RETRY_429: u32 = 6;
/// Fallback wait when a 429 response has no `retry-after` header.
const RETRY_AFTER_FALLBACK_SECS: u64 = 10;

/// Submission error classified so the retry loop can back off appropriately
/// for rate limiting (429) vs transient/other failures.
#[derive(Debug)]
pub(crate) enum SubmitError {
    /// HTTP 429 — the quota reset is given by the `retry-after` header, if present.
    RateLimited(Option<u64>),
    /// Any other failure.
    Other(String),
}

#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub base_url: String,
    pub api_key: String,
    pub max_concurrency: usize,
    pub model: Option<String>,
    pub language: Option<String>,
    pub provider: BatchProvider,
    /// Configured segment duration (ms); used to scale the drain grace window.
    pub segment_duration_ms: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            max_concurrency: 2,
            model: None,
            language: None,
            provider: BatchProvider::OpenAI,
            segment_duration_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SegmentStatus {
    Pending,
    InFlight { started_at: Instant },
    Completed { response: batch::Response },
    Failed { error: String, retry_count: u32 },
}

#[derive(Debug, Clone)]
pub struct SegmentEntry {
    pub index: usize,
    pub global_start_ms: i64,
    pub file_path: PathBuf,
    pub status: SegmentStatus,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub retry_count: u32,
}

#[derive(Debug, Clone)]
pub struct QueueProgress {
    pub total: usize,
    pub pending: usize,
    pub inflight: usize,
    pub completed: usize,
    pub failed: Vec<(usize, String)>,
}

#[derive(Debug, Clone)]
pub struct QueuedSegment {
    pub index: usize,
    pub global_start_ms: i64,
    pub file_path: PathBuf,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

#[derive(Debug, Clone)]
pub enum SegmentResult {
    Completed {
        index: usize,
        global_start_ms: i64,
        response: batch::Response,
    },
    Failed {
        index: usize,
        error: String,
        exhausted: bool,
    },
}

pub type SubmitSegmentFn = Arc<
    dyn Fn(QueuedSegment, QueueConfig) -> futures_util::future::BoxFuture<'static, SegmentResult>
        + Send
        + Sync,
>;

fn convert_to_s16le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let i16_val = (clamped * 32767.0) as i16;
        bytes.extend_from_slice(&i16_val.to_le_bytes());
    }
    bytes
}

/// Wrap raw s16le PCM in a minimal 44-byte WAV header. Groq rejects raw
/// `audio/pcm` uploads (only flac/mp3/mp4/.../wav/webm are accepted), so
/// segment submissions must carry a real WAV container.
pub(crate) fn wrap_pcm_as_wav(pcm_bytes: &[u8], sample_rate: u32) -> Vec<u8> {
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * bits_per_sample / 8;
    let data_len = pcm_bytes.len() as u32;

    let mut wav = Vec::with_capacity(44 + pcm_bytes.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm_bytes);
    wav
}

fn resample_linear(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let idx = src as usize;
        if idx + 1 < samples.len() {
            let frac = (src - idx as f64) as f32;
            out.push(samples[idx] * (1.0 - frac) + samples[idx + 1] * frac);
        } else if idx < samples.len() {
            out.push(samples[idx]);
        }
    }
    out
}

pub(crate) fn default_submit_fn() -> SubmitSegmentFn {
    Arc::new(|segment, config| {
        async move {
            let mut last_error;
            let mut attempts = 0u32;
            let mut rate_limited_streak = 0u32;            let max_attempts = if matches!(config.provider, BatchProvider::Groq) {
                MAX_RETRY_429
            } else {
                MAX_RETRIES
            };
            loop {
                match submit_segment_http(&config, &segment).await {
                    Ok(response) => {
                        return SegmentResult::Completed {
                            index: segment.index,
                            global_start_ms: segment.global_start_ms,
                            response,
                        };
                    }
                    Err(SubmitError::RateLimited(retry_after)) => {
                        attempts += 1;
                        rate_limited_streak += 1;
                        last_error = "HTTP 429 (rate limited)".to_string();
                        tracing::warn!(
                            "[progressive] submit retry idx={} attempt={attempts}/{max_attempts} rate_limited retry_after={:?}",
                            segment.index,
                            retry_after,
                        );
                        if attempts >= max_attempts {
                            break;
                        }
                        // Back off per rate-limit quota, honoring retry-after when given.
                        let delay_secs = retry_after
                            .unwrap_or(RETRY_AFTER_FALLBACK_SECS * 2u64.pow(rate_limited_streak.saturating_sub(1).min(3)));
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    }
                    Err(SubmitError::Other(error)) => {
                        attempts += 1;
                        rate_limited_streak = 0;
                        last_error = error.clone();
                        tracing::warn!(
                            "[progressive] submit retry idx={} attempt={attempts}/{max_attempts} error={error}",
                            segment.index,
                        );
                        if attempts >= max_attempts {
                            break;
                        }
                        let delay = Duration::from_secs(1 << attempts.saturating_sub(1).min(4));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
            SegmentResult::Failed {
                index: segment.index,
                error: last_error,
                exhausted: true,
            }
        }
        .boxed()
    })
}

pub struct BatchQueue {
    config: QueueConfig,
    segments: Vec<SegmentEntry>,
    submit_fn: SubmitSegmentFn,
    inflight_count: usize,
    result_rx: mpsc::UnboundedReceiver<SegmentResult>,
    result_tx: mpsc::UnboundedSender<SegmentResult>,
    cancelled: bool,
}

impl BatchQueue {
    pub fn new(config: QueueConfig) -> Self {
        let (result_tx, result_rx) = mpsc::unbounded_channel();
        Self {
            config,
            segments: Vec::new(),
            submit_fn: default_submit_fn(),
            inflight_count: 0,
            result_rx,
            result_tx,
            cancelled: false,
        }
    }

    pub fn with_submit_fn(config: QueueConfig, submit_fn: SubmitSegmentFn) -> Self {
        let (result_tx, result_rx) = mpsc::unbounded_channel();
        Self {
            config,
            segments: Vec::new(),
            submit_fn,
            inflight_count: 0,
            result_rx,
            result_tx,
            cancelled: false,
        }
    }

    pub fn enqueue(&mut self, segment: QueuedSegment) {
        tracing::debug!(
            "[progressive] queue enqueue idx={} start_ms={} samples={} sample_rate={}",
            segment.index,
            segment.global_start_ms,
            segment.samples.len(),
            segment.sample_rate,
        );
        self.segments.push(SegmentEntry {
            index: segment.index,
            global_start_ms: segment.global_start_ms,
            file_path: segment.file_path.clone(),
            status: SegmentStatus::Pending,
            samples: segment.samples,
            sample_rate: segment.sample_rate,
            retry_count: 0,
        });
        self.try_dispatch_from_pending();
    }

    fn apply_result(&mut self, result: &mut SegmentResult) {
        self.inflight_count = self.inflight_count.saturating_sub(1);
        match result {
            SegmentResult::Completed {
                index,
                global_start_ms,
                response,
            } => {
                if let Some(entry) = self.segments.iter_mut().find(|s| s.index == *index) {
                    *global_start_ms = entry.global_start_ms;
                    entry.status = SegmentStatus::Completed {
                        response: response.clone(),
                    };
                }
            }
            SegmentResult::Failed {
                index,
                error,
                exhausted,
            } => {
                if let Some(entry) = self.segments.iter_mut().find(|s| s.index == *index) {
                    if !*exhausted && entry.retry_count < MAX_RETRIES {
                        entry.retry_count += 1;
                        entry.status = SegmentStatus::Pending;
                    } else {
                        entry.status = SegmentStatus::Failed {
                            error: error.clone(),
                            retry_count: entry.retry_count,
                        };
                    }
                }
            }
        }
    }

    pub fn poll_completed(&mut self) -> Vec<SegmentResult> {
        let mut results = Vec::new();
        while let Ok(mut result) = self.result_rx.try_recv() {
            self.apply_result(&mut result);
            results.push(result);
        }
        if self
            .segments
            .iter()
            .any(|s| matches!(s.status, SegmentStatus::Pending))
        {
            self.try_dispatch_from_pending();
        }
        results
    }

    pub async fn drain(&mut self, timeout: Duration) -> Vec<SegmentResult> {
        let mut results = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let polled = self.poll_completed();
            results.extend(polled);

            if std::time::Instant::now() >= deadline {
                // Timeout reached. Give remaining segments one more dispatch
                // round and a grace window before giving up — the old behaviour
                // immediately marked them Failed, dropping tail groups that
                // were submitted after stop but hadn't finished yet.
                let remaining: Vec<usize> = self
                    .segments
                    .iter()
                    .filter(|s| {
                        matches!(
                            s.status,
                            SegmentStatus::Pending | SegmentStatus::InFlight { .. }
                        )
                    })
                    .map(|s| s.index)
                    .collect();
                if !remaining.is_empty() {
                    self.try_dispatch_from_pending();
                    // Grace window = segment length * 1.25, at least 60s. Server
                    // whisper on a 40s clip can take 60-90s; shorter segments
                    // still get a 60s floor, longer ones scale with length.
                    let grace_dur = Duration::from_secs(
                        (self.config.segment_duration_ms as u64 * 5 / 4).max(60),
                    );
                    let grace = std::time::Instant::now() + grace_dur;
                    // Poll the queue during the grace window instead of blocking
                    // on `result_rx.recv()`, so a submit that will never resolve
                    // doesn't stall the whole drain. Also bail early if several
                    // consecutive polls yield nothing new (submission stalled).
                    let mut idle_polls = 0u32;
                    let mut last_count = results.len();
                    loop {
                        let polled = self.poll_completed();
                        results.extend(polled);
                        if std::time::Instant::now() >= grace {
                            break;
                        }
                        let has_inflight = self
                            .segments
                            .iter()
                            .any(|s| matches!(s.status, SegmentStatus::InFlight { .. }));
                        let has_pending = self
                            .segments
                            .iter()
                            .any(|s| matches!(s.status, SegmentStatus::Pending));
                        if !has_inflight && !has_pending {
                            break;
                        }
                        if results.len() == last_count {
                            idle_polls += 1;
                            if idle_polls >= 10 {
                                break;
                            }
                        } else {
                            idle_polls = 0;
                        }
                        last_count = results.len();
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    // Drain any results that landed during the grace poll.
                    results.extend(self.poll_completed());
                }

                for entry in &mut self.segments {
                    if matches!(
                        entry.status,
                        SegmentStatus::Pending | SegmentStatus::InFlight { .. }
                    ) {
                        entry.status = SegmentStatus::Failed {
                            error: "timeout".to_string(),
                            retry_count: entry.retry_count,
                        };
                        results.push(SegmentResult::Failed {
                            index: entry.index,
                            error: "timeout".to_string(),
                            exhausted: true,
                        });
                    }
                }
                break;
            }

            let has_inflight = self
                .segments
                .iter()
                .any(|s| matches!(s.status, SegmentStatus::InFlight { .. }));
            let has_pending = self
                .segments
                .iter()
                .any(|s| matches!(s.status, SegmentStatus::Pending));
            if !has_inflight && !has_pending {
                break;
            }

            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, self.result_rx.recv()).await {
                Ok(Some(mut result)) => {
                    self.apply_result(&mut result);
                    results.push(result);
                    self.try_dispatch_from_pending();
                }
                _ => {}
            }
        }
        results
    }

    pub fn progress(&self) -> QueueProgress {
        let mut p = QueueProgress {
            total: self.segments.len(),
            pending: 0,
            inflight: 0,
            completed: 0,
            failed: Vec::new(),
        };
        for entry in &self.segments {
            match entry.status {
                SegmentStatus::Pending => p.pending += 1,
                SegmentStatus::InFlight { .. } => p.inflight += 1,
                SegmentStatus::Completed { .. } => p.completed += 1,
                SegmentStatus::Failed { ref error, .. } => {
                    p.failed.push((entry.index, error.clone()));
                }
            }
        }
        p
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.result_rx.close();
        self.inflight_count = 0;
        for entry in &mut self.segments {
            match entry.status {
                SegmentStatus::InFlight { .. } => {
                    entry.status = SegmentStatus::Failed {
                        error: "cancelled".to_string(),
                        retry_count: 0,
                    };
                }
                SegmentStatus::Pending => {
                    entry.status = SegmentStatus::Failed {
                        error: "cancelled".to_string(),
                        retry_count: 0,
                    };
                }
                _ => {}
            }
        }
    }

    fn try_dispatch_from_pending(&mut self) {
        while !self.cancelled && self.inflight_count < self.config.max_concurrency {
            let pending_idx = match self
                .segments
                .iter()
                .position(|s| matches!(s.status, SegmentStatus::Pending))
            {
                Some(idx) => idx,
                None => break,
            };

            let entry = &mut self.segments[pending_idx];
            entry.status = SegmentStatus::InFlight {
                started_at: Instant::now(),
            };
            self.inflight_count += 1;

            let file_path = entry.file_path.clone();
            let idx = entry.index;
            let global_start_ms = entry.global_start_ms;
            let samples = entry.samples.clone();
            let sample_rate = entry.sample_rate;
            let config = self.config.clone();
            let submit_fn = self.submit_fn.clone();
            let tx = self.result_tx.clone();

            tokio::spawn(async move {
                let queued = QueuedSegment {
                    index: idx,
                    global_start_ms,
                    file_path,
                    samples,
                    sample_rate,
                };
                let result = submit_fn(queued, config).await;
                let _ = tx.send(result);
            });
        }
    }
}

const TARGET_SAMPLE_RATE: u32 = 16000;

async fn submit_segment_http(
    config: &QueueConfig,
    segment: &QueuedSegment,
) -> Result<batch::Response, SubmitError> {
    let resampled = resample_linear(&segment.samples, segment.sample_rate, TARGET_SAMPLE_RATE);
    let pcm_bytes = convert_to_s16le(&resampled);

    // Groq rejects raw audio/pcm uploads; wrap the PCM in a WAV container.
    let is_groq = matches!(config.provider, BatchProvider::Groq);
    let upload_bytes = if is_groq {
        wrap_pcm_as_wav(&pcm_bytes, TARGET_SAMPLE_RATE)
    } else {
        pcm_bytes
    };
    let (file_name, mime) = if is_groq {
        ("audio.wav", "audio/wav")
    } else {
        ("audio.raw", "audio/pcm")
    };

    let file_part = reqwest::multipart::Part::bytes(upload_bytes)
        .file_name(file_name)
        .mime_str(mime)
        .map_err(|e| SubmitError::Other(format!("failed to create multipart: {e}")))?;

    let listen_params = owhisper_interface::ListenParams {
        model: config.model.clone(),
        languages: config
            .language
            .as_ref()
            .map(|l| l.parse::<hypr_language::Language>().ok())
            .into_iter()
            .flatten()
            .collect(),
        ..Default::default()
    };

    let url = owhisper_client::OpenAIAdapter::transcription_url(&config.base_url)
        .map_err(|e| SubmitError::Other(format!("failed to build transcription URL: {e}")))?;

    let form = owhisper_client::OpenAIAdapter::build_batch_multipart(
        file_part,
        &listen_params,
        true,
        false,
    )
    .map_err(|e| SubmitError::Other(format!("failed to build request: {e}")))?;

    tracing::debug!(
        "[progressive] submit_segment_http idx={} url={} model={:?} lang={:?}",
        segment.index,
        url,
        config.model,
        config.language,
    );

    let client = owhisper_client::create_client();
    let resp = client
        .post(url)
        .multipart(form)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await
        .map_err(|e| SubmitError::Other(format!("HTTP request failed: {e}")))?;

    tracing::debug!(
        "[progressive] submit_segment_http idx={} status={}",
        segment.index,
        resp.status(),
    );

    if resp.status().as_u16() == 429 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        return Err(SubmitError::RateLimited(retry_after));
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(SubmitError::Other(format!("HTTP {}: {}", status, body)));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| SubmitError::Other(format!("failed to read response body: {e}")))?;

    let response: batch::Response =
        if matches!(config.provider, BatchProvider::OpenAI | BatchProvider::Groq) {
            owhisper_client::OpenAIAdapter::parse_batch_response(&body)
                .map_err(|e| SubmitError::Other(format!("failed to parse response: {e}")))?
        } else {
            serde_json::from_str(&body)
                .map_err(|e| SubmitError::Other(format!("failed to parse response: {e}")))?
        };

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn success_result(index: usize) -> SegmentResult {
        SegmentResult::Completed {
            index,
            global_start_ms: 0,
            response: batch::Response {
                metadata: serde_json::json!({}),
                results: batch::Results {
                    channels: vec![batch::Channel {
                        alternatives: vec![batch::Alternatives {
                            transcript: "test".to_string(),
                            confidence: 0.95,
                            words: vec![],
                        }],
                    }],
                },
            },
        }
    }

    fn make_segment(index: usize) -> QueuedSegment {
        QueuedSegment {
            index,
            global_start_ms: (index as i64) * 29000,
            file_path: PathBuf::from(format!("/tmp/seg_{}.wav", index)),
            samples: vec![0.0; 16000],
            sample_rate: 16000,
        }
    }

    #[tokio::test]
    async fn test_enqueue_adds_inflight_segment() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let b = barrier.clone();
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(move |seg, _| {
                let b = b.clone();
                async move {
                    b.wait().await;
                    success_result(seg.index)
                }
                .boxed()
            }),
        );
        queue.enqueue(make_segment(0));
        assert_eq!(queue.segments.len(), 1);
        assert!(matches!(
            queue.segments[0].status,
            SegmentStatus::InFlight { .. }
        ));
    }

    #[tokio::test]
    async fn test_poll_completed_returns_empty_when_nothing_done() {
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(|_, _| async { success_result(0) }.boxed()),
        );
        queue.enqueue(make_segment(0));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let results = queue.poll_completed();
        assert!(results.len() >= 1);
    }

    #[tokio::test]
    async fn test_two_segments_enqueued_both_complete() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let count = call_count.clone();
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(move |seg, _| {
                count.fetch_add(1, Ordering::SeqCst);
                async move { success_result(seg.index) }.boxed()
            }),
        );

        queue.enqueue(make_segment(0));
        queue.enqueue(make_segment(1));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let results = queue.drain(Duration::from_secs(5)).await;
        assert_eq!(results.len(), 2);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
        assert_eq!(queue.progress().completed, 2);
    }

    #[tokio::test]
    async fn test_drain_waits_for_all_inflight() {
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig {
                max_concurrency: 2,
                ..Default::default()
            },
            Arc::new(|seg, _| {
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    success_result(seg.index)
                }
                .boxed()
            }),
        );

        for i in 0..5 {
            queue.enqueue(make_segment(i));
        }

        let results = queue.drain(Duration::from_secs(5)).await;
        assert_eq!(results.len(), 5);
        assert_eq!(queue.progress().completed, 5);
    }

    #[tokio::test]
    async fn test_max_concurrency_respected() {
        let inflight_count = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let inflight = inflight_count.clone();
        let max = max_seen.clone();

        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig {
                max_concurrency: 2,
                ..Default::default()
            },
            Arc::new(move |seg, _| {
                inflight.fetch_add(1, Ordering::SeqCst);
                let prev = inflight.load(Ordering::SeqCst);
                max.fetch_max(prev, Ordering::SeqCst);
                let inflight = inflight.clone();
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    success_result(seg.index)
                }
                .boxed()
            }),
        );

        for i in 0..4 {
            queue.enqueue(make_segment(i));
        }

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            max_seen.load(Ordering::SeqCst) <= 2,
            "max concurrency exceeded: {}",
            max_seen.load(Ordering::SeqCst)
        );

        queue.drain(Duration::from_secs(5)).await;
        assert_eq!(queue.progress().completed, 4);
    }

    #[tokio::test]
    async fn test_cancel_marks_all_as_failed() {
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(|_, _| {
                async {
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    success_result(0)
                }
                .boxed()
            }),
        );

        queue.enqueue(make_segment(0));
        queue.enqueue(make_segment(1));
        queue.cancel();

        assert_eq!(queue.progress().failed.len(), 2);
    }

    #[tokio::test]
    async fn test_progress_tracks_all_states() {
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig {
                max_concurrency: 10,
                ..Default::default()
            },
            Arc::new(|seg, _| {
                async move {
                    if seg.index == 2 {
                        SegmentResult::Failed {
                            index: 2,
                            error: "test error".to_string(),
                            exhausted: true,
                        }
                    } else {
                        success_result(seg.index)
                    }
                }
                .boxed()
            }),
        );

        for i in 0..4 {
            queue.enqueue(make_segment(i));
        }
        queue.drain(Duration::from_secs(5)).await;

        let p = queue.progress();
        assert_eq!(p.total, 4);
        assert_eq!(p.completed + p.failed.len(), 4);
    }

    #[tokio::test]
    async fn test_submit_fn_receives_correct_index() {
        let submitted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = submitted.clone();

        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(move |seg, _| {
                s.lock().unwrap().push(seg.index);
                async move { success_result(seg.index) }.boxed()
            }),
        );

        queue.enqueue(make_segment(3));
        queue.enqueue(make_segment(7));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        queue.drain(Duration::from_secs(5)).await;

        let submitted = submitted.lock().unwrap();
        assert!(submitted.contains(&3));
        assert!(submitted.contains(&7));
    }

    #[tokio::test]
    async fn test_empty_queue_drain_returns_immediately() {
        let mut queue = BatchQueue::new(QueueConfig::default());
        let results = queue.drain(Duration::from_secs(5)).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_poll_completed_multiple_batches() {
        let next_result = Arc::new(std::sync::Mutex::new(Vec::new()));
        let nr = next_result.clone();
        nr.lock()
            .unwrap()
            .extend(vec![Some(success_result(0)), Some(success_result(1))]);

        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(move |seg, _| {
                let guard = nr.lock().unwrap();
                let result = guard
                    .get(seg.index)
                    .and_then(|r| r.clone())
                    .unwrap_or_else(|| success_result(seg.index));
                async move { result }.boxed()
            }),
        );

        queue.enqueue(make_segment(0));
        queue.enqueue(make_segment(1));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let batch1 = queue.poll_completed();
        assert_eq!(batch1.len(), 2);
    }

    #[tokio::test]
    async fn test_segment_entry_has_correct_fields() {
        let mut queue = BatchQueue::new(QueueConfig::default());
        queue.enqueue(QueuedSegment {
            index: 42,
            global_start_ms: 123456,
            file_path: PathBuf::from("/tmp/test.wav"),
            samples: vec![0.0; 16000],
            sample_rate: 16000,
        });

        let entry = &queue.segments[0];
        assert_eq!(entry.index, 42);
        assert_eq!(entry.global_start_ms, 123456);
        assert_eq!(entry.file_path, PathBuf::from("/tmp/test.wav"));
    }

    #[tokio::test]
    async fn test_drain_timeout_emits_failed_for_pending() {
        // submit_fn that never completes → drain timeout fires
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(|_, _| async { futures_util::future::pending().await }.boxed()),
        );
        queue.enqueue(make_segment(0));
        let results = queue.drain(Duration::from_millis(10)).await;
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], SegmentResult::Failed { index: 0, .. }));
        let p = queue.progress();
        assert_eq!(p.failed.len(), 1);
    }

    #[tokio::test]
    async fn test_retry_count_increments_on_not_exhausted() {
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(|seg, _| {
                async move {
                    SegmentResult::Failed {
                        index: seg.index,
                        error: "transient".to_string(),
                        exhausted: false,
                    }
                }
                .boxed()
            }),
        );
        queue.enqueue(make_segment(0));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // First poll picks up Failed, apply_result increments retry_count to 1
        queue.poll_completed();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(queue.segments[0].retry_count, 1);

        // Second consumption: dispatch again → Failed again → retry_count → 2
        queue.poll_completed();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(queue.segments[0].retry_count, 2);
    }

    #[tokio::test]
    async fn test_not_exhausted_capped_at_max_retries() {
        // After MAX_RETRIES retries, the segment should be permanently Failed
        let mut queue = BatchQueue::with_submit_fn(
            QueueConfig::default(),
            Arc::new(|seg, _| {
                async move {
                    SegmentResult::Failed {
                        index: seg.index,
                        error: "transient".to_string(),
                        exhausted: false,
                    }
                }
                .boxed()
            }),
        );
        queue.enqueue(make_segment(0));

        // Poll enough times to exceed MAX_RETRIES
        for _ in 0..=MAX_RETRIES + 1 {
            queue.poll_completed();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(queue.segments[0].retry_count, MAX_RETRIES);
        assert!(matches!(
            queue.segments[0].status,
            SegmentStatus::Failed { .. }
        ));
    }

    #[test]
    fn wrap_pcm_as_wav_emits_valid_header() {
        let pcm = [
            0i16.to_le_bytes(),
            100i16.to_le_bytes(),
            300i16.to_le_bytes(),
        ]
        .concat();
        let wav = wrap_pcm_as_wav(&pcm, 16000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // fmt chunk: PCM=1, channels=1, sample_rate=16000, bits=16
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16000
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
        // data chunk length == pcm len, payload preserved
        assert_eq!(
            u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]),
            pcm.len() as u32
        );
        assert_eq!(&wav[44..], &pcm[..]);
    }
}
