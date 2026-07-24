#![allow(dead_code)]

mod integration;

pub use integration::run_progressive_batch_from_file;
mod queue;
mod segmenter;
mod stitcher;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use queue::{
    BatchQueue, QueueConfig, QueueProgress, QueuedSegment, SegmentResult, SubmitSegmentFn,
};
pub use segmenter::{AudioSegment, Segmenter, SegmenterConfig};
pub use stitcher::{CompletedSegment, Stitcher, StitcherConfig};

use owhisper_interface::batch;

use crate::batch::BatchProvider;

pub type PcmSender = tokio::sync::mpsc::Sender<Arc<[f32]>>;

pub type WriteWavFn = Arc<dyn Fn(&Path, &[f32], u32) -> Result<(), String> + Send + Sync>;

fn default_write_wav() -> WriteWavFn {
    Arc::new(
        |path: &Path, samples: &[f32], sample_rate: u32| -> Result<(), String> {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
            for &sample in samples {
                let amplitude = (sample * i16::MAX as f32) as i16;
                writer.write_sample(amplitude).map_err(|e| e.to_string())?;
            }
            writer.finalize().map_err(|e| e.to_string())?;
            Ok(())
        },
    )
}

#[derive(Debug, Clone)]
pub struct ProgressiveBatchConfig {
    pub sample_rate: u32,
    pub segment_duration_ms: u32,
    pub overlap_ms: u32,
    pub max_concurrency: usize,
    pub min_duration_secs: u32,
    pub base_url: String,
    pub api_key: String,
    pub model: Option<String>,
    pub language: Option<String>,
    pub provider: BatchProvider,
    pub session_dir: PathBuf,
}

impl Default for ProgressiveBatchConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            segment_duration_ms: 30000,
            overlap_ms: 1000,
            max_concurrency: 2,
            min_duration_secs: 180,
            base_url: String::new(),
            api_key: String::new(),
            model: None,
            language: None,
            provider: BatchProvider::OpenAI,
            session_dir: PathBuf::new(),
        }
    }
}

pub enum ManagerState {
    Accumulating {
        buffer: Vec<f32>,
        total_samples: u64,
    },
    Active {
        segmenter: Segmenter,
        queue: BatchQueue,
        stitcher: Stitcher,
    },
    Completed {
        result: batch::Response,
    },
    Failed {
        error: String,
    },
}

pub(crate) fn default_submit_fn() -> SubmitSegmentFn {
    queue::default_submit_fn()
}

pub struct ProgressiveBatchManager {
    config: ProgressiveBatchConfig,
    state: ManagerState,
    segments_dir: PathBuf,
    write_wav_fn: WriteWavFn,
    submit_fn: SubmitSegmentFn,
}

impl ProgressiveBatchManager {
    pub fn new(config: ProgressiveBatchConfig) -> Self {
        let segments_dir = config.session_dir.join("progressive-batch");
        if let Err(e) = std::fs::create_dir_all(&segments_dir) {
            tracing::warn!("failed to create segments dir: {e}");
        }
        Self {
            state: ManagerState::Accumulating {
                buffer: Vec::new(),
                total_samples: 0,
            },
            segments_dir,
            write_wav_fn: default_write_wav(),
            submit_fn: default_submit_fn(),
            config,
        }
    }

    #[cfg(test)]
    pub fn with_fns(
        config: ProgressiveBatchConfig,
        write_wav: WriteWavFn,
        submit: SubmitSegmentFn,
    ) -> Self {
        let segments_dir = config.session_dir.join("progressive-batch");
        let _ = std::fs::create_dir_all(&segments_dir);
        Self {
            state: ManagerState::Accumulating {
                buffer: Vec::new(),
                total_samples: 0,
            },
            segments_dir,
            write_wav_fn: write_wav,
            submit_fn: submit,
            config,
        }
    }

    pub fn state(&self) -> &ManagerState {
        &self.state
    }

    pub fn progress(&self) -> QueueProgress {
        match &self.state {
            ManagerState::Active { queue, .. } => queue.progress(),
            _ => QueueProgress {
                total: 0,
                pending: 0,
                inflight: 0,
                completed: 0,
                failed: vec![],
            },
        }
    }

    pub fn on_audio_frame(&mut self, samples: &[f32]) {
        let segments_dir = self.segments_dir.clone();
        let write_wav = self.write_wav_fn.clone();
        let sample_rate = self.config.sample_rate;

        let mut write_queue: Vec<(PathBuf, AudioSegment)> = Vec::new();
        let transition = match &mut self.state {
            ManagerState::Accumulating {
                buffer,
                total_samples,
            } => {
                buffer.extend_from_slice(samples);
                *total_samples += samples.len() as u64;
                let min = self.config.min_duration_secs as u64 * self.config.sample_rate as u64;
                if *total_samples >= min {
                    let buf = std::mem::take(buffer);
                    Some(buf)
                } else {
                    None
                }
            }
            ManagerState::Active {
                segmenter,
                queue,
                stitcher,
            } => {
                let segments = segmenter.feed(samples);
                for seg in segments {
                    let path = segments_dir.join(format!("seg_{}.wav", seg.index));
                    write_queue.push((path, seg));
                }
                for result in queue.poll_completed() {
                    if let SegmentResult::Completed {
                        index,
                        global_start_ms,
                        response,
                    } = result
                    {
                        stitcher.add_segment(CompletedSegment {
                            index,
                            global_start_ms,
                            response,
                        });
                    }
                }
                None
            }
            _ => None,
        };

        for (path, seg) in &write_queue {
            match write_wav(path, &seg.samples, sample_rate) {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!("failed to write segment {}: {e}", seg.index);
                }
            }
        }

        if let ManagerState::Active { queue, .. } = &mut self.state {
            for (path, seg) in write_queue {
                queue.enqueue(QueuedSegment {
                    index: seg.index,
                    global_start_ms: seg.global_start_ms,
                    file_path: path,
                    samples: seg.samples.clone(),
                    sample_rate,
                });
            }
        }

        if let Some(buf) = transition {
            let mut segmenter = Segmenter::new(SegmenterConfig {
                sample_rate: self.config.sample_rate,
                segment_duration_ms: self.config.segment_duration_ms,
                overlap_ms: self.config.overlap_ms,
            });
            let segments = segmenter.feed(&buf);
            let qc = self.queue_config();
            let mut queue = BatchQueue::with_submit_fn(qc, self.submit_fn.clone());
            let stitcher = Stitcher::new(StitcherConfig {
                overlap_ms: self.config.overlap_ms as u64,
                segment_duration_ms: self.config.segment_duration_ms as u64,
            });

            for seg in &segments {
                let path = self.segments_dir.join(format!("seg_{}.wav", seg.index));
                if let Err(e) = (self.write_wav_fn)(&path, &seg.samples, self.config.sample_rate) {
                    tracing::warn!("failed to write segment {}: {e}", seg.index);
                }
                queue.enqueue(QueuedSegment {
                    index: seg.index,
                    global_start_ms: seg.global_start_ms,
                    file_path: path,
                    samples: seg.samples.clone(),
                    sample_rate: self.config.sample_rate,
                });
            }

            self.state = ManagerState::Active {
                segmenter,
                queue,
                stitcher,
            };
        }
    }

    pub async fn finish(&mut self) -> Result<batch::Response, String> {
        let old_state = std::mem::replace(
            &mut self.state,
            ManagerState::Failed {
                error: "transitioning".to_string(),
            },
        );

        let (mut queue, mut stitcher) = match old_state {
            ManagerState::Accumulating { buffer, .. } => {
                if buffer.is_empty() {
                    self.state = ManagerState::Failed {
                        error: "no audio recorded".to_string(),
                    };
                    return Err("no audio recorded".to_string());
                }
                let mut segmenter = Segmenter::new(SegmenterConfig {
                    sample_rate: self.config.sample_rate,
                    segment_duration_ms: self.config.segment_duration_ms,
                    overlap_ms: self.config.overlap_ms,
                });
                let feed_segments = segmenter.feed(&buffer);
                let flushed = segmenter.flush();
                let qc = self.queue_config();
                let sc = StitcherConfig {
                    overlap_ms: self.config.overlap_ms as u64,
                    segment_duration_ms: self.config.segment_duration_ms as u64,
                };
                let mut queue = BatchQueue::with_submit_fn(qc, self.submit_fn.clone());
                let stitcher = Stitcher::new(sc);
                for seg in feed_segments.iter().chain(flushed.iter()) {
                    self.write_and_enqueue(seg, &mut queue);
                }
                (queue, stitcher)
            }
            ManagerState::Active {
                mut segmenter,
                mut queue,
                stitcher,
            } => {
                let flushed = segmenter.flush();
                for seg in &flushed {
                    self.write_and_enqueue(seg, &mut queue);
                }
                (queue, stitcher)
            }
            ManagerState::Completed { result } => {
                self.state = ManagerState::Completed {
                    result: result.clone(),
                };
                return Ok(result);
            }
            ManagerState::Failed { error } => {
                self.state = ManagerState::Failed {
                    error: error.clone(),
                };
                return Err(error);
            }
        };

        let drain_results = queue.drain().await;

        // Check for permanent failures before stitching
        let qp = queue.progress();
        if !qp.failed.is_empty() {
            let errors: Vec<String> = qp
                .failed
                .iter()
                .map(|(idx, e)| format!("seg {idx}: {e}"))
                .collect();
            let msg = format!("segment failures: {}", errors.join("; "));
            self.state = ManagerState::Failed { error: msg.clone() };
            return Err(msg);
        }

        for result in drain_results.into_iter().chain(queue.poll_completed()) {
            if let SegmentResult::Completed {
                index,
                global_start_ms,
                response,
            } = result
            {
                stitcher.add_segment(CompletedSegment {
                    index,
                    global_start_ms,
                    response,
                });
            }
        }

        if stitcher.is_complete() {
            match stitcher.stitch() {
                Ok(result) => {
                    self.state = ManagerState::Completed {
                        result: result.clone(),
                    };
                    let _ = std::fs::remove_dir_all(&self.segments_dir);
                    Ok(result)
                }
                Err(e) => {
                    let msg = format!("stitch failed: {e:?}");
                    self.state = ManagerState::Failed { error: msg.clone() };
                    Err(msg)
                }
            }
        } else {
            let msg = "not all segments completed".to_string();
            self.state = ManagerState::Failed { error: msg.clone() };
            Err(msg)
        }
    }

    fn queue_config(&self) -> QueueConfig {
        QueueConfig {
            base_url: self.config.base_url.clone(),
            api_key: self.config.api_key.clone(),
            max_concurrency: self.config.max_concurrency,
            model: self.config.model.clone(),
            language: self.config.language.clone(),
            provider: self.config.provider.clone(),
        }
    }

    fn write_and_enqueue(&self, segment: &AudioSegment, queue: &mut BatchQueue) {
        let path = self.segments_dir.join(format!("seg_{}.wav", segment.index));
        if let Err(e) = (self.write_wav_fn)(&path, &segment.samples, self.config.sample_rate) {
            tracing::warn!("failed to write segment {}: {e}", segment.index);
        }
        queue.enqueue(QueuedSegment {
            index: segment.index,
            global_start_ms: segment.global_start_ms,
            file_path: path,
            samples: segment.samples.clone(),
            sample_rate: self.config.sample_rate,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use std::sync::Mutex;

    fn mock_write_wav() -> WriteWavFn {
        Arc::new(|_: &Path, _: &[f32], _: u32| Ok(()))
    }

    fn mock_submit_success() -> SubmitSegmentFn {
        Arc::new(|seg: QueuedSegment, _: QueueConfig| {
            async move {
                SegmentResult::Completed {
                    index: seg.index,
                    global_start_ms: seg.global_start_ms,
                    response: batch::Response {
                        metadata: serde_json::json!({}),
                        results: batch::Results {
                            channels: vec![batch::Channel {
                                alternatives: vec![batch::Alternatives {
                                    transcript: "test".to_string(),
                                    confidence: 0.95,
                                    words: vec![batch::Word {
                                        word: "test".to_string(),
                                        start: 0.0,
                                        end: 1.0,
                                        confidence: 0.95,
                                        channel: 0,
                                        speaker: None,
                                        punctuated_word: Some("test".to_string()),
                                    }],
                                }],
                            }],
                        },
                    },
                }
            }
            .boxed()
        })
    }

    fn test_config() -> ProgressiveBatchConfig {
        ProgressiveBatchConfig {
            sample_rate: 1000,
            segment_duration_ms: 100,
            overlap_ms: 10,
            max_concurrency: 2,
            min_duration_secs: 1,
            base_url: "http://localhost:8080/v1".to_string(),
            api_key: "test-key".to_string(),
            model: None,
            language: None,
            session_dir: PathBuf::from("/tmp/progressive-test"),
            ..Default::default()
        }
    }

    #[test]
    fn test_initial_state_is_accumulating() {
        let manager = ProgressiveBatchManager::with_fns(
            test_config(),
            mock_write_wav(),
            mock_submit_success(),
        );
        assert!(matches!(manager.state(), ManagerState::Accumulating { .. }));
    }

    #[test]
    fn test_accumulating_below_threshold_stays() {
        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            min_duration_secs: 10,
            ..test_config()
        };
        let mut manager =
            ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), mock_submit_success());
        manager.on_audio_frame(&[0.0; 100]);
        assert!(matches!(manager.state(), ManagerState::Accumulating { .. }));
    }

    #[tokio::test]
    async fn test_accumulating_above_threshold_transitions_to_active() {
        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            min_duration_secs: 1,
            segment_duration_ms: 100,
            overlap_ms: 10,
            max_concurrency: 2,
            ..test_config()
        };
        let mut manager =
            ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), mock_submit_success());
        manager.on_audio_frame(&[0.0; 1000]);
        assert!(matches!(manager.state(), ManagerState::Active { .. }));
    }

    #[tokio::test]
    async fn test_finish_in_accumulating_empty_errors() {
        let mut manager = ProgressiveBatchManager::with_fns(
            test_config(),
            mock_write_wav(),
            mock_submit_success(),
        );
        let result = manager.finish().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_finish_in_active_drains_and_completes() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let w = written.clone();
        let write_fn: WriteWavFn = Arc::new(move |path: &Path, _: &[f32], _: u32| {
            w.lock().unwrap().push(path.to_path_buf());
            Ok(())
        });

        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            min_duration_secs: 1,
            segment_duration_ms: 100,
            overlap_ms: 10,
            max_concurrency: 2,
            session_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..test_config()
        };

        let mut manager = ProgressiveBatchManager::with_fns(cfg, write_fn, mock_submit_success());
        manager.on_audio_frame(&[0.0; 1000]);
        assert!(matches!(manager.state(), ManagerState::Active { .. }));

        let result = manager.finish().await;
        assert!(result.is_ok(), "finish failed: {:?}", result.err());
        assert!(matches!(manager.state(), ManagerState::Completed { .. }));
    }

    #[test]
    fn test_on_audio_frame_noop_in_completed() {
        let mut manager = ProgressiveBatchManager::with_fns(
            test_config(),
            mock_write_wav(),
            mock_submit_success(),
        );
        manager.state = ManagerState::Completed {
            result: batch::Response {
                metadata: serde_json::json!({}),
                results: batch::Results { channels: vec![] },
            },
        };
        manager.on_audio_frame(&[0.0; 10]);
        assert!(matches!(manager.state(), ManagerState::Completed { .. }));
    }

    #[test]
    fn test_progress_returns_zero_for_accumulating() {
        let manager = ProgressiveBatchManager::with_fns(
            test_config(),
            mock_write_wav(),
            mock_submit_success(),
        );
        let p = manager.progress();
        assert_eq!(p.total, 0);
    }

    #[tokio::test]
    async fn test_double_finish_returns_same_result() {
        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            min_duration_secs: 1,
            segment_duration_ms: 100,
            overlap_ms: 10,
            max_concurrency: 2,
            session_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..test_config()
        };
        let mut manager =
            ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), mock_submit_success());
        manager.on_audio_frame(&[0.0; 1000]);
        let r1 = manager.finish().await;
        let r2 = manager.finish().await;
        assert!(r1.is_ok());
        assert!(r2.is_ok());
    }

    #[tokio::test]
    async fn test_on_audio_frame_cumulative_before_active() {
        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            min_duration_secs: 3,
            segment_duration_ms: 100,
            overlap_ms: 10,
            ..test_config()
        };
        let mut manager =
            ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), mock_submit_success());
        for _ in 0..2 {
            manager.on_audio_frame(&[0.0; 1000]);
            assert!(matches!(manager.state(), ManagerState::Accumulating { .. }));
        }
        manager.on_audio_frame(&[0.0; 1000]);
        assert!(matches!(manager.state(), ManagerState::Active { .. }));
    }

    #[tokio::test]
    async fn test_simulated_speaches_multi_segment() {
        const SR: u32 = 48000;
        const SEG_MS: u32 = 30000;
        const OVL_MS: u32 = 1000;
        const STRIDE_MS: u32 = SEG_MS - OVL_MS;

        // Realistic Chinese text derived from Speaches Realtime API output
        // (测试 session 7f365064 and earlier recordings).
        // Each segment: 10 regular words spaced ~2.9s apart covering [0, 29) +
        // 2 overlap words in [29, 30) that duplicate the next segment's first 2.
        //
        // After dedup: seg 0 keeps 12, seg 1-4 each keep 12 (14 - 2 overlap at each boundary).
        let segment_phrases: Vec<Vec<(&str, f64, f64)>> = vec![
            // seg 0 (10 reg + 2 overlap)
            vec![
                ("产品", 0.0, 2.9),
                ("配置", 2.9, 5.8),
                ("方案", 5.8, 8.7),
                ("零十二", 8.7, 11.6),
                ("零配置", 11.6, 14.5),
                ("数据", 14.5, 17.4),
                ("中心", 17.4, 20.3),
                ("终于", 20.3, 23.2),
                ("青岛", 23.2, 26.1),
                ("清水", 26.1, 29.0),
                ("中心", 29.0, 29.5),
                ("是不是", 29.5, 30.0),
            ],
            // seg 1
            vec![
                ("中心", 0.0, 0.5),
                ("是不是", 0.5, 1.0),
                ("很", 1.0, 3.8),
                ("兴趣", 3.8, 6.6),
                ("嗎", 6.6, 9.4),
                ("有個", 9.4, 12.2),
                ("Settings", 12.2, 15.0),
                ("是", 15.0, 17.8),
                ("什么", 17.8, 20.6),
                ("所以", 20.6, 23.4),
                ("我", 23.4, 26.2),
                ("心裡", 26.2, 29.0),
                ("心", 29.0, 29.5),
                ("就要", 29.5, 30.0),
            ],
            // seg 2
            vec![
                ("心", 0.0, 0.5),
                ("就要", 0.5, 1.0),
                ("做到", 1.0, 3.8),
                ("这一点", 3.8, 6.6),
                ("是不是", 6.6, 9.4),
                ("我们", 9.4, 12.2),
                ("下一个", 12.2, 15.0),
                ("视频", 15.0, 17.8),
                ("再见", 17.8, 20.6),
                ("字幕", 20.6, 23.4),
                ("by", 23.4, 26.2),
                ("索兰娅", 26.2, 29.0),
                ("那么", 29.0, 29.5),
                ("最终", 29.5, 30.0),
            ],
            // seg 3
            vec![
                ("那么", 0.0, 0.5),
                ("最终", 0.5, 1.0),
                ("我们", 1.0, 3.8),
                ("可能", 3.8, 6.6),
                ("因为", 6.6, 9.4),
                ("一个人", 9.4, 12.2),
                ("还没", 12.2, 15.0),
                ("个人", 15.0, 17.8),
                ("我可能", 17.8, 20.6),
                ("拨了", 20.6, 23.4),
                ("一个地方", 23.4, 26.2),
                ("是个", 26.2, 29.0),
                ("小小的", 29.0, 29.5),
                ("我们", 29.5, 30.0),
            ],
            // seg 4 (trailing overlap kept — no next segment)
            vec![
                ("小小的", 0.0, 0.5),
                ("我们", 0.5, 1.0),
                ("在这边", 1.0, 3.8),
                ("我们可能", 3.8, 6.6),
                ("这样子", 6.6, 9.4),
                ("是三个", 9.4, 12.2),
                ("小时", 12.2, 15.0),
                ("我们就", 15.0, 17.8),
                ("开始", 17.8, 20.6),
                ("睡觉", 20.6, 23.4),
                ("那你说", 23.4, 26.2),
                ("我们", 26.2, 29.0),
                ("拨了", 29.0, 29.5),
                ("一个小时", 29.5, 30.0),
            ],
        ];

        // Pre-compute each segment's response with correct global offset
        let mut prebuilt = std::collections::HashMap::new();
        for (seg_idx, phrases) in segment_phrases.iter().enumerate() {
            // Words are in LOCAL time (relative to segment start).
            // Stitcher adds global_start_ms offset automatically.
            let words: Vec<batch::Word> = phrases
                .iter()
                .map(|(text, start, end)| batch::Word {
                    word: text.to_string(),
                    start: *start,
                    end: *end,
                    confidence: 0.95,
                    channel: 0,
                    speaker: None,
                    punctuated_word: Some(text.to_string()),
                })
                .collect();

            let transcript = words
                .iter()
                .map(|w| w.punctuated_word.as_deref().unwrap_or(&w.word))
                .collect::<Vec<_>>()
                .join(" ");

            prebuilt.insert(
                seg_idx,
                batch::Response {
                    metadata: serde_json::json!({ "segment_index": seg_idx }),
                    results: batch::Results {
                        channels: vec![batch::Channel {
                            alternatives: vec![batch::Alternatives {
                                transcript,
                                confidence: 0.95,
                                words,
                            }],
                        }],
                    },
                },
            );
        }

        let prebuilt = Arc::new(prebuilt);

        let submit_fn: SubmitSegmentFn = {
            let prebuilt = prebuilt.clone();
            Arc::new(move |seg: QueuedSegment, _: QueueConfig| {
                let prebuilt = prebuilt.clone();
                async move {
                    let response = prebuilt.get(&seg.index).cloned().unwrap_or_else(|| {
                        // Flush segment or unknown index → empty words (stitcher skips)
                        batch::Response {
                            metadata: serde_json::json!({}),
                            results: batch::Results {
                                channels: vec![batch::Channel {
                                    alternatives: vec![batch::Alternatives {
                                        transcript: String::new(),
                                        confidence: 0.0,
                                        words: vec![],
                                    }],
                                }],
                            },
                        }
                    });
                    SegmentResult::Completed {
                        index: seg.index,
                        global_start_ms: seg.global_start_ms,
                        response,
                    }
                }
                .boxed()
            })
        };

        let session_dir = tempfile::tempdir().unwrap();
        let cfg = ProgressiveBatchConfig {
            sample_rate: SR,
            segment_duration_ms: SEG_MS,
            overlap_ms: OVL_MS,
            max_concurrency: 2,
            min_duration_secs: 1,
            session_dir: session_dir.path().to_path_buf(),
            ..Default::default()
        };

        let mut manager = ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), submit_fn);

        // Feed audio to produce exactly 5 full segments
        // stride = 29s/samples, segment first = 30s, then 4 strides = 116s → total 146s
        let total_samples = (STRIDE_MS as u64 * 4 + SEG_MS as u64) as usize * SR as usize / 1000;
        let chunk_size = SR as usize; // 1s chunks
        for chunk_start in (0..total_samples).step_by(chunk_size) {
            let end = std::cmp::min(chunk_start + chunk_size, total_samples);
            let len = end - chunk_start;
            manager.on_audio_frame(&vec![0.0f32; len]);
        }

        // Verify progress while still in Active state
        let progress = manager.progress();
        assert!(progress.total >= 5, "at least 5 segments enqueued");
        assert!(progress.failed.is_empty());

        let result = manager.finish().await.expect("finish should succeed");
        let alt = &result.results.channels[0].alternatives[0];
        let words = &alt.words;

        // After dedup: seg 0 keeps 12, seg 1-4 each keep 12 (14 - 2 overlap)
        assert_eq!(words.len(), 60, "expected 60 unique words after dedup");

        // Monotonically increasing timestamps
        for i in 1..words.len() {
            assert!(
                words[i].start >= words[i - 1].end - 0.1,
                "word {i} start {:.3} < prev end {:.3} (\"{}\" after \"{}\")",
                words[i].start,
                words[i - 1].end,
                words[i].word,
                words[i - 1].word,
            );
        }

        // First word at ~0s, last word end at ~146s
        assert!((words[0].start - 0.0).abs() < 0.01, "first word start");
        let expected_last_end = 146.0;
        assert!(
            (words.last().unwrap().end - expected_last_end).abs() < 0.1,
            "last word end {:.3} != {:.3}",
            words.last().unwrap().end,
            expected_last_end,
        );

        // Verify segment boundaries: each segment contributes 12 words after dedup
        for seg_idx in 0..5 {
            let expected_global_ms = seg_idx as i64 * STRIDE_MS as i64;
            let expected_global_s = expected_global_ms as f64 / 1000.0;
            let seg_phrases = &segment_phrases[seg_idx];

            // After dedup, seg 0 keeps all words; seg 1-4 drop first 2 (overlap)
            let keep_from = if seg_idx == 0 { 0 } else { 2 };
            let kept_phrases = &seg_phrases[keep_from..];
            let output_base = seg_idx * 12;

            for (local_idx, (text, local_start, local_end)) in kept_phrases.iter().enumerate() {
                let expected_start = local_start + expected_global_s;
                let expected_end = local_end + expected_global_s;
                let output_idx = output_base + local_idx;

                assert_eq!(
                    words[output_idx].word, *text,
                    "seg {seg_idx} word {local_idx} text mismatch"
                );
                assert!(
                    (words[output_idx].start - expected_start).abs() < 0.05,
                    "seg {seg_idx} word {local_idx} \"{text}\" start {:.3} != expected {:.3}",
                    words[output_idx].start,
                    expected_start,
                );
                assert!(
                    (words[output_idx].end - expected_end).abs() < 0.05,
                    "seg {seg_idx} word {local_idx} \"{text}\" end {:.3} != expected {:.3}",
                    words[output_idx].end,
                    expected_end,
                );
            }
        }

        // Verify metadata
        // 6 segments total: 5 with words + 1 flush segment (empty words, skipped by stitcher)
        assert_eq!(
            result.metadata["segments_stitched"].as_u64(),
            Some(6),
            "6 segments (5 data + 1 flush) were stitched"
        );
        assert!(
            result.metadata.get("gap_warnings").is_none(),
            "no gap warnings expected for contiguous segments"
        );

        // Progress is no longer available once manager transitions to Completed
    }

    #[tokio::test]
    async fn test_finish_from_accumulating_submits_all_audio() {
        // Previously this path discarded feed() segments and only flushed.
        // Now it chains feed_segments + flushed so all audio is submitted.
        let written = Arc::new(Mutex::new(Vec::new()));
        let w = written.clone();
        let write_fn: WriteWavFn = Arc::new(move |path: &Path, _: &[f32], _: u32| {
            w.lock().unwrap().push(path.to_path_buf());
            Ok(())
        });

        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            segment_duration_ms: 100,
            overlap_ms: 10,
            max_concurrency: 10,
            min_duration_secs: 10,
            session_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..test_config()
        };

        let mut manager = ProgressiveBatchManager::with_fns(cfg, write_fn, mock_submit_success());
        // 500 samples at 1000Hz = 0.5s — below min_duration_secs (10s), stays Accumulating
        manager.on_audio_frame(&[0.0; 500]);
        assert!(matches!(manager.state(), ManagerState::Accumulating { .. }));

        // finish() should create a segmenter, feed the 500 samples, flush, and submit
        let result = manager.finish().await;
        assert!(
            result.is_ok(),
            "finish from accumulating failed: {:?}",
            result.err()
        );
        assert!(matches!(manager.state(), ManagerState::Completed { .. }));

        // The stitched result should contain the word from mock_submit_success
        let words = &result.unwrap().results.channels[0].alternatives[0].words;
        assert!(!words.is_empty(), "should have at least one word");
    }

    #[tokio::test]
    async fn test_progressive_multi_chunk_in_active() {
        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            segment_duration_ms: 100,
            overlap_ms: 10,
            max_concurrency: 10,
            min_duration_secs: 1,
            session_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..test_config()
        };

        let mut manager =
            ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), mock_submit_success());
        // Feed enough to trigger Active (1000 samples at 1000Hz = 1s)
        manager.on_audio_frame(&[0.0; 1000]);
        assert!(matches!(manager.state(), ManagerState::Active { .. }));

        let progress_before = manager.progress();
        assert!(progress_before.total > 0, "should have segments enqueued");

        // Feed more audio in multiple small chunks to exercise progressive segmenter
        for _ in 0..5 {
            manager.on_audio_frame(&[0.0; 100]);
            // Yield to let async submit tasks process
            tokio::task::yield_now().await;
        }

        let progress_mid = manager.progress();
        assert!(
            progress_mid.total > progress_before.total,
            "more segments should be enqueued after additional audio: {} <= {}",
            progress_mid.total,
            progress_before.total,
        );

        let result = manager.finish().await;
        assert!(result.is_ok(), "finish failed: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_submit_retry_on_transient_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let call_count = Arc::new(AtomicUsize::new(0));

        let submit_fn: SubmitSegmentFn = {
            let count = call_count.clone();
            Arc::new(move |seg: QueuedSegment, _: QueueConfig| {
                let prev = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if prev == 0 {
                        // First call fails transiently; BatchQueue will retry
                        SegmentResult::Failed {
                            index: seg.index,
                            error: "transient error".to_string(),
                            exhausted: false,
                        }
                    } else {
                        SegmentResult::Completed {
                            index: seg.index,
                            global_start_ms: seg.global_start_ms,
                            response: batch::Response {
                                metadata: serde_json::json!({}),
                                results: batch::Results {
                                    channels: vec![batch::Channel {
                                        alternatives: vec![batch::Alternatives {
                                            transcript: "retried".to_string(),
                                            confidence: 0.95,
                                            words: vec![batch::Word {
                                                word: "retried".to_string(),
                                                start: 0.0,
                                                end: 1.0,
                                                confidence: 0.95,
                                                channel: 0,
                                                speaker: None,
                                                punctuated_word: Some("retried".to_string()),
                                            }],
                                        }],
                                    }],
                                },
                            },
                        }
                    }
                }
                .boxed()
            })
        };

        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            segment_duration_ms: 100,
            overlap_ms: 10,
            max_concurrency: 2,
            min_duration_secs: 1,
            session_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..test_config()
        };

        let mut manager = ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), submit_fn);
        manager.on_audio_frame(&[0.0; 1000]);
        assert!(matches!(manager.state(), ManagerState::Active { .. }));

        let result = manager.finish().await;
        assert!(
            result.is_ok(),
            "finish should succeed after retry: {:?}",
            result.err()
        );

        // The submit function should have been called at least twice (first fails, then retried)
        assert!(
            call_count.load(Ordering::SeqCst) >= 2,
            "submit should have been retried"
        );
    }

    #[tokio::test]
    async fn test_partial_failure_returns_error() {
        let submit_fn: SubmitSegmentFn = Arc::new(|seg: QueuedSegment, _: QueueConfig| {
            async move {
                if seg.index == 1 {
                    // Segment 1 permanently fails
                    SegmentResult::Failed {
                        index: seg.index,
                        error: "permanent error".to_string(),
                        exhausted: true,
                    }
                } else {
                    SegmentResult::Completed {
                        index: seg.index,
                        global_start_ms: seg.global_start_ms,
                        response: batch::Response {
                            metadata: serde_json::json!({}),
                            results: batch::Results {
                                channels: vec![batch::Channel {
                                    alternatives: vec![batch::Alternatives {
                                        transcript: "ok".to_string(),
                                        confidence: 0.95,
                                        words: vec![batch::Word {
                                            word: "ok".to_string(),
                                            start: 0.0,
                                            end: 1.0,
                                            confidence: 0.95,
                                            channel: 0,
                                            speaker: None,
                                            punctuated_word: Some("ok".to_string()),
                                        }],
                                    }],
                                }],
                            },
                        },
                    }
                }
            }
            .boxed()
        });

        // Use bigger segments to ensure exactly 2 segments
        // With 1000Hz, 1000ms segments, 0 overlap, stride = 1000
        // 1500 samples → 1 full segment + 500 in flush
        // We need 2 segments with index 0 and 1 to test partial failure
        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            segment_duration_ms: 500,
            overlap_ms: 0,
            max_concurrency: 2,
            min_duration_secs: 1,
            session_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..test_config()
        };

        let mut manager = ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), submit_fn);
        manager.on_audio_frame(&[0.0; 1000]);
        assert!(matches!(manager.state(), ManagerState::Active { .. }));

        let result = manager.finish().await;
        assert!(
            result.is_err(),
            "finish should fail when a segment permanently fails"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("segment failures"),
            "error should mention segment failures: {err}"
        );
        assert!(matches!(manager.state(), ManagerState::Failed { .. }));
    }

    #[tokio::test]
    async fn test_zero_overlap_happy_path() {
        // With overlap_ms = 0, each segment covers disjoint audio.
        // No dedup should occur — every word from every segment appears once.
        let cfg = ProgressiveBatchConfig {
            sample_rate: 1000,
            segment_duration_ms: 100,
            overlap_ms: 0,
            max_concurrency: 2,
            min_duration_secs: 1,
            session_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            ..test_config()
        };

        let mut manager =
            ProgressiveBatchManager::with_fns(cfg, mock_write_wav(), mock_submit_success());
        // Feed 1200 samples = enough to trigger Active (min 1s=1000)
        // With 100ms segments and 0 overlap: stride = 100
        // 1200 samples → 12 feed segments (indices 0..11)
        manager.on_audio_frame(&[0.0; 1200]);
        assert!(matches!(manager.state(), ManagerState::Active { .. }));

        let result = manager.finish().await;
        assert!(result.is_ok(), "finish failed: {:?}", result.err());

        let words = &result.unwrap().results.channels[0].alternatives[0].words;

        // Each segment has 1 word "test" (from mock_submit_success).
        // 12 feed segments, no flush (buffer fully consumed with 0 overlap)
        // With zero overlap, no words are deduped.
        assert_eq!(words.len(), 12, "12 segments × 1 word each, no dedup");
        for w in words {
            assert_eq!(w.word, "test");
        }
    }
}
