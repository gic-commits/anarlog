use std::path::Path;

use crate::clustering;
use crate::embedding_providers::{EmbeddingProvider, create_provider_from_path};
use crate::segmentation::{Segment, Segmenter};

#[derive(Debug, Clone)]
pub struct DiarizationConfig {
    pub model_path: Option<String>,
    pub threshold: f32,
    pub min_segment_duration_s: f32,
    pub sample_rate: u32,
}

impl Default for DiarizationConfig {
    fn default() -> Self {
        Self {
            model_path: None,
            threshold: 0.35,
            min_segment_duration_s: 1.5,
            sample_rate: 16000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpeakerSegment {
    pub start: f64,
    pub end: f64,
    pub speaker: usize,
    pub embedding_valid: bool,
}

#[derive(Debug, Clone)]
pub struct DiarizationResult {
    pub segments: Vec<SpeakerSegment>,
    pub n_speakers: usize,
}

pub struct DiarizationManager {
    segmenter: Segmenter,
    provider: Box<dyn EmbeddingProvider>,
    config: DiarizationConfig,
}

impl DiarizationManager {
    /// 创建新的 DiarizationManager
    ///
    /// `model_path`: ONNX 模型路径。如果为 `None`，使用内置 pyannote-local 模型。
    pub fn new(config: DiarizationConfig) -> Result<Self, crate::Error> {
        let segmenter = Segmenter::new(config.sample_rate)?;

        let provider: Box<dyn EmbeddingProvider> = match &config.model_path {
            Some(path) => {
                let path = Path::new(path);
                if path.exists() {
                    create_provider_from_path(path)?
                } else {
                    eprintln!(
                        "[pyannote-local] model not found: {path:?}, falling back to pyannote-local"
                    );
                    Box::new(crate::embedding::PyannoteEmbeddingProvider::new())
                }
            }
            None => Box::new(crate::embedding::PyannoteEmbeddingProvider::new()),
        };

        Ok(Self {
            segmenter,
            provider,
            config,
        })
    }

    /// 处理音频，返回染色结果
    ///
    /// `audio_i16`: PCM s16le 格式，采样率 `config.sample_rate`
    pub fn process(&mut self, audio_i16: &[i16]) -> Result<DiarizationResult, crate::Error> {
        let segments = self.segmenter.process(audio_i16, self.config.sample_rate)?;

        if segments.is_empty() {
            return Ok(DiarizationResult {
                segments: vec![],
                n_speakers: 0,
            });
        }

        // Step 1: 对所有 segments 提取 embedding
        let embedded = self.extract_all(&segments);

        // Step 2: 短段合并
        let merged = self.merge_short_segments(&segments, &embedded);

        // Step 3: 聚类
        let assignments = self.cluster_assignments(&merged);

        // Step 4: 构建结果
        let result_segments: Vec<SpeakerSegment> = merged
            .iter()
            .enumerate()
            .map(|(i, seg)| SpeakerSegment {
                start: seg.start,
                end: seg.end,
                speaker: if assignments.is_empty() {
                    0
                } else {
                    assignments[i]
                },
                embedding_valid: seg.embedding_valid,
            })
            .collect();

        let n_speakers = if assignments.is_empty() {
            0
        } else {
            assignments.iter().max().unwrap_or(&0) + 1
        };

        Ok(DiarizationResult {
            segments: result_segments,
            n_speakers,
        })
    }

    fn extract_all(&mut self, segments: &[Segment]) -> Vec<Option<Vec<f32>>> {
        segments
            .iter()
            .map(|seg| {
                let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
                self.provider.compute(&f32_s).ok().flatten()
            })
            .collect()
    }

    /// 合并短段（< min_duration_s）：合并到相邻的最近有效段
    fn merge_short_segments(
        &self,
        segments: &[Segment],
        embeddings: &[Option<Vec<f32>>],
    ) -> Vec<MergedSegment> {
        let min_samples =
            (self.config.min_segment_duration_s * self.config.sample_rate as f32) as usize;
        let n = segments.len();

        // Determine which segments are too short
        let mut too_short: Vec<bool> = Vec::with_capacity(n);
        for (i, seg) in segments.iter().enumerate() {
            let is_short = seg.samples.len() < min_samples || embeddings[i].is_none();
            too_short.push(is_short);
        }

        // Merge short segments with neighbors
        let mut merged: Vec<MergedSegment> = Vec::new();
        let mut i = 0;
        while i < n {
            if !too_short[i] {
                // Valid segment — start a group
                let mut after: Vec<usize> = Vec::new();
                // Look ahead for consecutive short segments after this
                let mut j = i + 1;
                while j < n && too_short[j] {
                    after.push(j);
                    j += 1;
                }
                // The main valid segment absorbs following short segments
                let mut seg = MergedSegment {
                    start: segments[i].start,
                    end: segments[j - 1].end,
                    samples: segments[i].samples.clone(),
                    embedding: embeddings[i].clone(),
                    embedding_valid: true,
                };
                // Append following short segments' samples
                for &k in &after {
                    seg.samples.extend_from_slice(&segments[k].samples);
                    seg.end = segments[k].end;
                }
                merged.push(seg);
                i = j;
            } else {
                // Short segment at start or isolated — merge with next valid
                let mut k = i + 1;
                while k < n && too_short[k] {
                    k += 1;
                }
                if k < n {
                    // Merge with next valid segment
                    let mut samples = segments[i].samples.clone();
                    let mut end = segments[i].end;
                    for t in (i + 1)..=k {
                        samples.extend_from_slice(&segments[t].samples);
                        end = segments[t].end;
                    }
                    merged.push(MergedSegment {
                        start: segments[i].start,
                        end,
                        samples,
                        embedding: embeddings[k].clone(),
                        embedding_valid: true,
                    });
                    i = k + 1;
                } else {
                    // No next valid segment — push as-is (won't have embedding)
                    merged.push(MergedSegment {
                        start: segments[i].start,
                        end: segments[i].end,
                        samples: segments[i].samples.clone(),
                        embedding: embeddings[i].clone(),
                        embedding_valid: false,
                    });
                    i += 1;
                }
            }
        }

        merged
    }

    fn cluster_assignments(&self, merged: &[MergedSegment]) -> Vec<usize> {
        let valid_embs: Vec<Vec<f32>> = merged
            .iter()
            .filter_map(|seg| seg.embedding.clone())
            .collect();

        if valid_embs.len() < 2 {
            return vec![0; merged.len()];
        }

        let cids = clustering::cluster_embeddings(&valid_embs, self.config.threshold);

        // Map back to all segments (short ones inherit the nearest cluster)
        let mut assignments = Vec::with_capacity(merged.len());
        let mut vi = 0;
        for seg in merged {
            if seg.embedding_valid {
                assignments.push(cids[vi]);
                vi += 1;
            } else {
                // Short segment: inherit from previous valid if exists
                let prev = if assignments.is_empty() {
                    0
                } else {
                    assignments[assignments.len() - 1]
                };
                assignments.push(prev);
            }
        }

        assignments
    }
}

#[derive(Debug, Clone)]
struct MergedSegment {
    start: f64,
    end: f64,
    samples: Vec<i16>,
    embedding: Option<Vec<f32>>,
    embedding_valid: bool,
}
