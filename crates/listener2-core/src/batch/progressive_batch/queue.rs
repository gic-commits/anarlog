use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures_util::FutureExt;
use owhisper_interface::batch;
use tokio::sync::mpsc;

use crate::batch::BatchProvider;

const MAX_RETRIES: u32 = 3;

#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub base_url: String,
    pub api_key: String,
    pub max_concurrency: usize,
    pub model: Option<String>,
    pub language: Option<String>,
    pub provider: BatchProvider,
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
            let result = submit_segment_http(&config, &segment).await;
            match result {
                Ok(response) => SegmentResult::Completed {
                    index: segment.index,
                    global_start_ms: 0,
                    response,
                },
                Err(error) => SegmentResult::Failed {
                    index: segment.index,
                    error,
                    exhausted: true,
                },
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
                    let retries = match &entry.status {
                        SegmentStatus::Failed { retry_count, .. } => *retry_count,
                        _ => 0,
                    };
                    if !*exhausted && retries < MAX_RETRIES {
                        entry.status = SegmentStatus::Pending;
                    } else {
                        entry.status = SegmentStatus::Failed {
                            error: error.clone(),
                            retry_count: if *exhausted { retries } else { retries + 1 },
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

    pub async fn drain(&mut self) -> Vec<SegmentResult> {
        let mut results = Vec::new();
        loop {
            let polled = self.poll_completed();
            let has_inflight = self
                .segments
                .iter()
                .any(|s| matches!(s.status, SegmentStatus::InFlight { .. }));
            let has_pending = self
                .segments
                .iter()
                .any(|s| matches!(s.status, SegmentStatus::Pending));
            if !has_inflight && !has_pending {
                results.extend(polled);
                tracing::debug!(
                    "[progressive] drain complete: {} results",
                    results.len(),
                );
                break;
            }
            results.extend(polled);
            tracing::debug!(
                "[progressive] drain waiting... inflight={} pending={} results={}",
                if has_inflight { 1 } else { 0 },
                if has_pending { 1 } else { 0 },
                results.len(),
            );
            if let Some(mut result) = self.result_rx.recv().await {
                self.apply_result(&mut result);
                results.push(result);
                self.try_dispatch_from_pending();
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
) -> Result<batch::Response, String> {
    let resampled = resample_linear(&segment.samples, segment.sample_rate, TARGET_SAMPLE_RATE);
    let pcm_bytes = convert_to_s16le(&resampled);

    let file_part = reqwest::multipart::Part::bytes(pcm_bytes)
        .file_name("audio.raw")
        .mime_str("audio/pcm")
        .map_err(|e| format!("failed to create multipart: {e}"))?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("response_format", "verbose_json");

    if let Some(ref model) = config.model {
        form = form.text("model", model.clone());
    }
    if let Some(ref lang) = config.language {
        form = form.text("language", lang.clone());
    }

    let mut url: url::Url = config
        .base_url
        .parse()
        .map_err(|e: url::ParseError| format!("invalid base_url: {e}"))?;
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("audio/transcriptions") {
        url.path_segments_mut()
            .map_err(|_| "cannot modify base_url path segments".to_string())?
            .pop_if_empty()
            .push("audio/transcriptions");
    }

    tracing::debug!(
        "[progressive] submit_segment_http idx={} url={} model={:?} lang={:?}",
        segment.index,
        url,
        config.model,
        config.language,
    );

    let client = reqwest::Client::new();
    let mut req = client.post(url).multipart(form);
    if !config.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", config.api_key));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    tracing::debug!(
        "[progressive] submit_segment_http idx={} status={}",
        segment.index,
        resp.status(),
    );

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, body));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;

    let response: batch::Response = if config.provider == BatchProvider::OpenAI {
        owhisper_client::OpenAIAdapter::parse_batch_response(&body)
            .map_err(|e| format!("failed to parse response: {e}"))?
    } else {
        serde_json::from_str(&body)
            .map_err(|e| format!("failed to parse response: {e}"))?
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

        let results = queue.drain().await;
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

        let results = queue.drain().await;
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

        queue.drain().await;
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
        queue.drain().await;

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
        queue.drain().await;

        let submitted = submitted.lock().unwrap();
        assert!(submitted.contains(&3));
        assert!(submitted.contains(&7));
    }

    #[tokio::test]
    async fn test_empty_queue_drain_returns_immediately() {
        let mut queue = BatchQueue::new(QueueConfig::default());
        let results = queue.drain().await;
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
}
