use crate::clustering;
use crate::diarization::SpeakerSegment;
use crate::embedding_providers::{EmbeddingProvider, create_provider_from_path};
use crate::incremental_vad::IncrementalVad;
use crate::segmentation::Segment;

const MIN_EMBEDDING_SAMPLES: usize = 16000;

#[derive(Debug, Clone)]
pub struct IncrementalDiarizationConfig {
    pub sample_rate: u32,
    pub model_path: Option<String>,
    pub threshold: f32,
    pub recluster_interval: usize,
}

impl Default for IncrementalDiarizationConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            model_path: None,
            threshold: 0.85,
            recluster_interval: 5,
        }
    }
}

struct VocSeg {
    start: f64,
    end: f64,
    speaker: usize,
    embedding: Option<Vec<f32>>,
    valid: bool,
}

pub struct IncrementalDiarizationEngine {
    vad: IncrementalVad,
    provider: Box<dyn EmbeddingProvider>,
    config: IncrementalDiarizationConfig,
    segments: Vec<VocSeg>,
    since_last_cluster: usize,
}

impl IncrementalDiarizationEngine {
    pub fn new(config: IncrementalDiarizationConfig) -> Result<Self, crate::Error> {
        let vad = IncrementalVad::new(config.sample_rate)?;

        let provider: Box<dyn EmbeddingProvider> = match &config.model_path {
            Some(path_str) => {
                let path = resolve_model_path(path_str);
                if let Some(p) = path {
                    match create_provider_from_path(&p) {
                        Ok(provider) => provider,
                        Err(e) => {
                            eprintln!(
                                "[pyannote-local] WARNING: failed to load model from '{}': {}, falling back to built-in pyannote model",
                                p.display(),
                                e
                            );
                            Box::new(crate::embedding::PyannoteEmbeddingProvider::new())
                        }
                    }
                } else {
                    eprintln!(
                        "[pyannote-local] WARNING: diarization model '{}' not found, falling back to built-in pyannote model (lower quality)",
                        path_str
                    );
                    Box::new(crate::embedding::PyannoteEmbeddingProvider::new())
                }
            }
            None => {
                eprintln!(
                    "[pyannote-local] WARNING: no diarization model configured, using built-in pyannote model (lower quality)"
                );
                Box::new(crate::embedding::PyannoteEmbeddingProvider::new())
            }
        };

        Ok(Self {
            vad,
            provider,
            config,
            segments: Vec::new(),
            since_last_cluster: 0,
        })
    }

    /// Feed PCM (s16le, `sample_rate` Hz), returns newly completed VAD segments.
    pub fn feed_pcm(&mut self, samples: &[i16]) -> Result<Vec<Segment>, crate::Error> {
        let new_segs = self.vad.feed(samples)?;

        for seg in &new_segs {
            let embedding = if seg.samples.len() >= MIN_EMBEDDING_SAMPLES {
                let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
                self.provider.compute(&f32_s).ok().flatten()
            } else {
                None
            };

            let valid = embedding.is_some();
            self.segments.push(VocSeg {
                start: seg.start,
                end: seg.end,
                speaker: self.segments.len(),
                embedding,
                valid,
            });

            self.since_last_cluster += 1;
        }

        if self.since_last_cluster >= self.config.recluster_interval && !self.segments.is_empty() {
            self.recluster();
            self.since_last_cluster = 0;
        }

        Ok(new_segs)
    }

    /// Feed pre-computed VAD segments for speaker embedding and clustering.
    /// Bypasses the engine's internal VAD — use instead of `feed_pcm` when
    /// you already have VAD segments from an external segmenter (e.g. Segmenter).
    pub fn feed_segments(&mut self, segments: &[Segment]) -> Result<(), crate::Error> {
        for seg in segments {
            let embedding = if seg.samples.len() >= MIN_EMBEDDING_SAMPLES {
                let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
                self.provider.compute(&f32_s).ok().flatten()
            } else {
                None
            };
            let valid = embedding.is_some();
            self.segments.push(VocSeg {
                start: seg.start,
                end: seg.end,
                speaker: self.segments.len(),
                embedding,
                valid,
            });
            self.since_last_cluster += 1;
        }
        if self.since_last_cluster >= self.config.recluster_interval && !self.segments.is_empty() {
            self.recluster();
            self.since_last_cluster = 0;
        }
        Ok(())
    }

    /// Final re-cluster after all audio has been fed.
    pub fn finalize(&mut self) {
        for seg in self.vad.finish() {
            let embedding = if seg.samples.len() >= MIN_EMBEDDING_SAMPLES {
                let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
                self.provider.compute(&f32_s).ok().flatten()
            } else {
                None
            };
            let valid = embedding.is_some();
            self.segments.push(VocSeg {
                start: seg.start,
                end: seg.end,
                speaker: self.segments.len(),
                embedding,
                valid,
            });
        }
        if !self.segments.is_empty() {
            self.recluster();
        }
    }

    /// Name of the embedding provider/model in use.
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// Speaker label for a given timestamp (seconds).
    /// Uses half-open interval [start, end) to avoid boundary ambiguity.
    /// When the timestamp falls in a silence gap (no segment), returns None
    /// and the caller (propagate_speaker_to_none) fills from neighbor words.
    pub fn speaker_at_time(&self, time_s: f64) -> Option<usize> {
        for seg in &self.segments {
            if seg.start <= time_s && time_s < seg.end {
                return Some(seg.speaker);
            }
        }
        None
    }

    /// All VAD segments with speaker labels.
    pub fn speaker_segments(&self) -> Vec<SpeakerSegment> {
        self.segments
            .iter()
            .map(|s| SpeakerSegment {
                start: s.start,
                end: s.end,
                speaker: s.speaker,
                embedding_valid: s.valid,
            })
            .collect()
    }

    fn recluster(&mut self) {
        let valid_embs: Vec<Vec<f32>> = self
            .segments
            .iter()
            .filter_map(|s| s.embedding.clone())
            .collect();

        if valid_embs.len() < 2 {
            return;
        }

        let cids = clustering::cluster_embeddings(&valid_embs, self.config.threshold);

        let mut vi = 0;
        for seg in &mut self.segments {
            if seg.valid {
                if vi < cids.len() {
                    seg.speaker = cids[vi];
                }
                vi += 1;
            }
        }

        // Propagate speaker from nearest valid segment to short/invalid ones.
        let n = self.segments.len();
        for i in 0..n {
            if self.segments[i].valid {
                continue;
            }
            let spk = self.nearest_valid_speaker(i).unwrap_or(0);
            self.segments[i].speaker = spk;
        }
    }

    fn nearest_valid_speaker(&self, idx: usize) -> Option<usize> {
        let center = |s: &VocSeg| (s.start + s.end) / 2.0;
        let target = center(&self.segments[idx]);
        let mut best: Option<(usize, f64)> = None;
        for (i, seg) in self.segments.iter().enumerate() {
            if i == idx || !seg.valid {
                continue;
            }
            let dist = (center(seg) - target).abs();
            if best.map_or(true, |(_, bd)| dist < bd) {
                best = Some((seg.speaker, dist));
            }
        }
        best.map(|(spk, _)| spk)
    }
}

const TEST_MODELS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/models");

/// Resolve a model path by trying several locations:
/// 1. As-is
/// 2. Relative to the test models directory (development)
fn resolve_model_path(path_str: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path_str);
    if p.exists() {
        return Some(p.to_path_buf());
    }

    // Try relative to known test models directory
    let test_path = std::path::Path::new(TEST_MODELS_DIR).join(path_str);
    if test_path.exists() {
        return Some(test_path);
    }

    // Try "models/" subdirectory of cwd
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_path = cwd.join("models").join(path_str);
        if cwd_path.exists() {
            return Some(cwd_path);
        }
    }

    None
}
