use crate::clustering;
use crate::diarization::SpeakerSegment;
use crate::embedding_providers::{EmbeddingProvider, create_provider_from_path};
use crate::incremental_vad::IncrementalVad;
use crate::segmentation::Segment;

const MIN_EMBEDDING_SAMPLES: usize = 16000;
const MIN_PAUSE_SECS: f64 = 0.2;
const MAX_CHUNK_SECS: f64 = 4.0;

#[derive(Debug, Clone)]
pub struct IncrementalDiarizationConfig {
    pub sample_rate: u32,
    pub model_path: Option<String>,
    pub threshold: f32,
    pub recluster_interval: usize,
    /// Minimum cumulative duration (seconds) for a speaker to be kept as
    /// "dominant". Speakers below this are candidates for reassignment.
    pub min_speaker_secs: f64,
    /// Minimum contiguous span (seconds) for a speaker to be kept even if its
    /// cumulative duration is small — a real short turn is contiguous, while
    /// per-chunk embedding noise fragments are scattered.
    pub min_contiguous_span_secs: f64,
    /// Max gap (seconds) between adjacent chunks of the same speaker before
    /// the contiguous-span run is considered broken.
    pub span_gap_secs: f64,
}

impl Default for IncrementalDiarizationConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            model_path: None,
            threshold: 0.5,
            recluster_interval: 5,
            min_speaker_secs: 8.0,
            min_contiguous_span_secs: 5.0,
            span_gap_secs: 2.0,
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
            self.feed_one_segment(seg);
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
            self.feed_one_segment(seg);
        }
        if self.since_last_cluster >= self.config.recluster_interval && !self.segments.is_empty() {
            self.recluster();
            self.since_last_cluster = 0;
        }
        Ok(())
    }

    /// Split a VAD segment into turn-aligned sub-chunks so each embedding
    /// covers a single speaker turn region. Long VAD segments from continuous
    /// group speech blend multiple speakers into one average embedding;
    /// sub-chunking at energy pauses recovers per-turn speaker separation.
    fn feed_one_segment(&mut self, seg: &Segment) {
        let sample_rate = self.config.sample_rate as f64;
        let chunks = crate::min_cut_merge::split_into_turn_chunks(
            &seg.samples,
            seg.start,
            self.config.sample_rate,
            MIN_PAUSE_SECS,
            MAX_CHUNK_SECS,
        );

        // Periodic diagnostics: report chunking on the first segment and then
        // roughly every 100 segments so we can see how many turn-chunks each
        // VAD segment splits into (many → over-segmentation risk).
        if self.segments.is_empty() || self.segments.len() % 100 == 0 {
            eprintln!(
                "[diarization] feed_one_segment: vad=[{:.1}s, {:.1}s] dur={:.1}s -> {} turn-chunks (total_voc={})",
                seg.start,
                seg.end,
                seg.end - seg.start,
                chunks.len(),
                self.segments.len() + chunks.len(),
            );
        }

        for (start_s, end_s) in chunks {
            let lo = ((start_s - seg.start) * sample_rate) as usize;
            let hi = (((end_s - seg.start) * sample_rate) as usize).min(seg.samples.len());
            if hi - lo < MIN_EMBEDDING_SAMPLES {
                continue;
            }
            let sub = &seg.samples[lo..hi];
            let f32_s: Vec<f32> = sub.iter().map(|&s| s as f32 / 32768.0).collect();
            let embedding = self.provider.compute(&f32_s).ok().flatten();
            let valid = embedding.is_some();
            self.segments.push(VocSeg {
                start: start_s,
                end: end_s,
                speaker: self.segments.len(),
                embedding,
                valid,
            });
            self.since_last_cluster += 1;
        }
    }

    /// Final re-cluster after all audio has been fed.
    pub fn finalize(&mut self) {
        for seg in self.vad.finish() {
            self.feed_one_segment(&seg);
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
        // Skip invalid segments (no valid embedding) and the HDBSCAN noise
        // sentinel (`usize::MAX - i`), which would otherwise leak as a
        // garbage speaker label into word annotations when smooth passes
        // didn't absorb the noise chunk.
        const NOISE_SENTINEL_FLOOR: usize = usize::MAX - 1_000_000;
        for seg in &self.segments {
            if seg.valid
                && seg.speaker < NOISE_SENTINEL_FLOOR
                && seg.start <= time_s
                && time_s < seg.end
            {
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

        // HDBSCAN density clustering (WeSpeaker recipe) on cosine distance:
        // finds clusters of varying density automatically and labels outliers
        // as -1, which the downstream smooth_speakers/merge passes absorb.
        // Unlike the fixed-threshold agglomerative clustering, this does not
        // over-segment long recordings into dozens of fake speakers.
        const MIN_CLUSTER_SIZE: usize = 3;
        let cids_i32 = clustering::hdbscan_cluster_embeddings(&valid_embs, MIN_CLUSTER_SIZE);

        // -1 (noise) chunks keep their pre-cluster label (temporarily use the
        // index) so smooth_speakers can reassign them to temporal neighbors.
        let cids: Vec<usize> = cids_i32
            .iter()
            .enumerate()
            .map(|(i, &c)| if c >= 0 { c as usize } else { usize::MAX - i })
            .collect();

        let mut vi = 0;
        for seg in &mut self.segments {
            if seg.valid {
                if vi < cids.len() {
                    seg.speaker = cids[vi];
                }
                vi += 1;
            }
        }

        // Per-chunk embeddings are noisy; a single 1-3s chunk can land in its
        // own cluster surrounded by a dominant speaker. Smooth by majority
        // vote over a time window, so brief misassigned chunks get absorbed
        // into their temporal neighbors.
        self.smooth_speakers();

        // Merge A-B-A flicker turns (a speaker's short occurrences sandwiched
        // between the same other speaker) — this is a person's voice split
        // into a few chunks by clustering, not a real turn change.
        self.merge_sandwiched_short_turns();

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

    /// Reassign each valid segment to the majority speaker among valid
    /// segments whose centers fall within `SMOOTH_WINDOW_SECS` of its center.
    fn smooth_speakers(&mut self) {
        let n = self.segments.len();
        if n < 3 {
            return;
        }

        // Keep a speaker if it is either "dominant" (large cumulative duration)
        // or has a real contiguous turn (a run of chunks with gaps under
        // `span_gap_secs` whose total span reaches `min_contiguous_span_secs`).
        // Per-chunk embedding noise fragments are scattered (short contiguous
        // runs with large gaps between them), so they fail the contiguous-span
        // test and get reassigned to the nearest kept speaker.
        let mut cumulative: std::collections::HashMap<usize, f64> =
            std::collections::HashMap::new();
        let mut chunks: std::collections::HashMap<usize, Vec<(f64, f64)>> =
            std::collections::HashMap::new();
        for seg in &self.segments {
            if seg.valid {
                *cumulative.entry(seg.speaker).or_insert(0.0) += seg.end - seg.start;
                chunks
                    .entry(seg.speaker)
                    .or_insert_with(Vec::new)
                    .push((seg.start, seg.end));
            }
        }

        let max_contiguous_span = |chunks: &[(f64, f64)]| -> f64 {
            let mut sorted = chunks.to_vec();
            sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let mut best = 0.0f64;
            let mut run_start = sorted.first().map(|c| c.0).unwrap_or(0.0);
            let mut run_end = sorted.first().map(|c| c.1).unwrap_or(0.0);
            for &(s, e) in &sorted {
                if s - run_end <= self.config.span_gap_secs {
                    run_end = run_end.max(e);
                } else {
                    best = best.max(run_end - run_start);
                    run_start = s;
                    run_end = e;
                }
            }
            best.max(run_end - run_start)
        };

        let keep: std::collections::HashSet<usize> = chunks
            .iter()
            .filter(|(spk, ch)| {
                let cum = cumulative.get(spk).copied().unwrap_or(0.0);
                cum >= self.config.min_speaker_secs
                    || max_contiguous_span(ch) >= self.config.min_contiguous_span_secs
            })
            .map(|(&s, _)| s)
            .collect();

        if keep.len() == chunks.len() {
            return;
        }

        // Reassign dropped speakers to their temporally nearest kept speaker.
        let speakers: Vec<usize> = self.segments.iter().map(|s| s.speaker).collect();
        let mut reassigned: Vec<usize> = Vec::with_capacity(n);
        for (i, seg) in self.segments.iter().enumerate() {
            if !seg.valid || keep.contains(&seg.speaker) {
                reassigned.push(speakers[i]);
                continue;
            }
            let center = (seg.start + seg.end) / 2.0;
            let mut best: Option<(usize, f64)> = None;
            for (j, other) in self.segments.iter().enumerate() {
                if i == j || !other.valid || !keep.contains(&other.speaker) {
                    continue;
                }
                let d = ((other.start + other.end) / 2.0 - center).abs();
                if best.map_or(true, |(_, bd)| d < bd) {
                    best = Some((other.speaker, d));
                }
            }
            reassigned.push(best.map(|(s, _)| s).unwrap_or(speakers[i]));
        }
        for (i, spk) in reassigned.into_iter().enumerate() {
            self.segments[i].speaker = spk;
        }
    }

    /// Merge "flicker" speakers: a speaker whose individual occurrences are
    /// short and sandwiched between the SAME other speaker (A-B-A pattern).
    /// One person's voice split by clustering into a few chunks produces this
    /// pattern — the brief B segments are reassigned to the surrounding A.
    fn merge_sandwiched_short_turns(&mut self) {
        const SWITCH_MAX_SECS: f64 = 4.0;

        let n = self.segments.len();
        if n < 3 {
            return;
        }

        // Track how much total time a speaker occupies in short sandwiched
        // turns, plus whether the enclosing speaker is consistent (A-B-A).
        // First pass: collect per-speaker short-turn spans.
        let mut short_turn_span: std::collections::HashMap<usize, f64> =
            std::collections::HashMap::new();
        let mut candidate: std::collections::HashMap<usize, Option<usize>> =
            std::collections::HashMap::new();
        for i in 0..n {
            let seg = &self.segments[i];
            if !seg.valid {
                continue;
            }
            let span = seg.end - seg.start;
            if span > SWITCH_MAX_SECS {
                continue;
            }
            // Find enclosing non-self speakers.
            let mut prev_spk: Option<usize> = None;
            for j in (0..i).rev() {
                if self.segments[j].valid && self.segments[j].speaker != seg.speaker {
                    prev_spk = Some(self.segments[j].speaker);
                    break;
                }
            }
            let mut next_spk: Option<usize> = None;
            for j in i + 1..n {
                if self.segments[j].valid && self.segments[j].speaker != seg.speaker {
                    next_spk = Some(self.segments[j].speaker);
                    break;
                }
            }
            // A-B-A: prev and next are the same speaker and within GUARD.
            if let (Some(p), Some(nx)) = (prev_spk, next_spk)
                && p == nx
            {
                let entry = candidate.entry(seg.speaker).or_insert(Some(p));
                if *entry != Some(p) {
                    *entry = None; // ambiguous — different enclosers
                }
                *short_turn_span.entry(seg.speaker).or_insert(0.0) += span;
            }
        }

        // A speaker whose total short-sandwiched time is small is a flicker.
        const MAX_TOTAL_SHORT_SECS: f64 = 12.0;
        for (spk, encloser) in candidate.iter() {
            let Some(encloser) = encloser else {
                continue;
            };
            let total = short_turn_span.get(spk).copied().unwrap_or(0.0);
            if total >= MAX_TOTAL_SHORT_SECS {
                continue;
            }
            for seg in &mut self.segments {
                if seg.valid && seg.speaker == *spk {
                    seg.speaker = *encloser;
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_segments(specs: &[(f64, f64, usize, bool)]) -> IncrementalDiarizationEngine {
        let mut engine =
            IncrementalDiarizationEngine::new(IncrementalDiarizationConfig::default()).unwrap();
        engine.segments = specs
            .iter()
            .map(|&(start, end, speaker, valid)| VocSeg {
                start,
                end,
                speaker,
                embedding: valid.then(|| vec![0.0; 4]),
                valid,
            })
            .collect();
        engine
    }

    #[test]
    fn test_smooth_keeps_dominant_speakers() {
        // spk0: 10s total (two chunks), spk1: brief 1.5s noise chunk.
        let mut engine = engine_with_segments(&[
            (0.0, 4.0, 0, true),
            (4.0, 10.0, 0, true),
            (10.0, 11.5, 1, true),
            (11.5, 16.0, 0, true),
        ]);
        engine.smooth_speakers();
        // spk1 (1.5s < 2.0s) reassigned to spk0.
        assert!(engine.segments.iter().all(|s| s.speaker == 0));
    }

    #[test]
    fn test_smooth_keeps_real_interleaved_speaker() {
        // spk0 main (12s), spk1 real secondary: contiguous 6s turn → kept by
        // the contiguous-span condition even though cumulative < min_speaker_secs.
        let mut engine = engine_with_segments(&[
            (0.0, 5.0, 0, true),
            (5.0, 8.0, 1, true),
            (8.0, 11.0, 1, true),
            (11.0, 15.0, 0, true),
        ]);
        engine.smooth_speakers();
        let speakers: std::collections::HashSet<usize> =
            engine.segments.iter().map(|s| s.speaker).collect();
        assert_eq!(speakers.len(), 2, "both real speakers retained");
        assert!(speakers.contains(&0) && speakers.contains(&1));
    }

    #[test]
    fn test_smooth_filters_scattered_noise_speaker() {
        // spk0 dominant (12s). spk1: 3 tiny scattered chunks, each far apart
        // (> span_gap) and cumulative < min_speaker_secs → filtered.
        let mut engine = engine_with_segments(&[
            (0.0, 4.0, 0, true),
            (4.0, 8.0, 0, true),
            (8.0, 12.0, 0, true),
            (20.0, 22.0, 1, true),
            (40.0, 42.0, 1, true),
            (60.0, 62.0, 1, true),
        ]);
        engine.smooth_speakers();
        assert!(engine.segments.iter().all(|s| s.speaker == 0));
    }

    #[test]
    fn test_smooth_noop_when_all_above_min() {
        let mut engine = engine_with_segments(&[
            (0.0, 4.0, 0, true),
            (4.0, 8.0, 0, true),
            (8.0, 12.0, 1, true),
            (12.0, 16.0, 1, true),
        ]);
        let before: Vec<usize> = engine.segments.iter().map(|s| s.speaker).collect();
        engine.smooth_speakers();
        let after: Vec<usize> = engine.segments.iter().map(|s| s.speaker).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn test_merge_sandwiched_short_turn() {
        // A-B-A flicker: spk1 appears briefly (1s) sandwiched between spk0.
        let mut engine = engine_with_segments(&[
            (0.0, 4.0, 0, true),
            (4.0, 5.0, 1, true),
            (5.0, 9.0, 0, true),
        ]);
        engine.merge_sandwiched_short_turns();
        assert!(engine.segments.iter().all(|s| s.speaker == 0));
    }

    #[test]
    fn test_merge_does_not_touch_real_interleaved_turn() {
        // spk1 has a real 3s turn between different speakers (not A-B-A) —
        // must be preserved.
        let mut engine = engine_with_segments(&[
            (0.0, 4.0, 0, true),
            (4.0, 7.0, 1, true),
            (7.0, 11.0, 2, true),
        ]);
        let before: Vec<usize> = engine.segments.iter().map(|s| s.speaker).collect();
        engine.merge_sandwiched_short_turns();
        let after: Vec<usize> = engine.segments.iter().map(|s| s.speaker).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn test_merge_keeps_recurring_short_speaker() {
        // spk1 recurs as short 1s sandwiched turns 14 times → total 14s
        // (> MAX_TOTAL_SHORT_SECS) so it's a real recurring participant.
        let mut specs = vec![(0.0, 4.0, 0, true)];
        for i in 0..14 {
            let base = 4.0 + i as f64 * 2.0;
            specs.push((base, base + 1.0, 1, true));
            specs.push((base + 1.0, base + 2.0, 0, true));
        }
        let mut engine = engine_with_segments(&specs);
        engine.merge_sandwiched_short_turns();
        // spk1 must survive (14s cumulative short turns > 12s threshold).
        assert!(engine.segments.iter().any(|s| s.speaker == 1));
    }
}
