use hypr_pyannote_local::incremental_vad::IncrementalVad;
use hypr_pyannote_local::min_cut_merge::split_segment_at_energy;
use hypr_pyannote_local::segmentation::Segment;

use super::{AudioSegment, SegmentProducer};

#[derive(Debug, Clone)]
pub struct VadGroupStreamConfig {
    pub sample_rate: u32,
    pub max_duration_ms: u32,
    /// Retain emitted VAD segments so the live path can feed them to
    /// diarization (matching the file path). Off by default to avoid holding
    /// speech samples when no diarization consumer exists.
    pub collect_vad_segments: bool,
}

impl Default for VadGroupStreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            max_duration_ms: 30000,
            collect_vad_segments: false,
        }
    }
}

/// Streaming VAD source producing raw speech `Segment`s (absolute times,
/// own samples). Mirrors the batch `Segmenter::process` output so the live
/// path groups exactly like the file re-transcribe path.
pub trait VadSource: Send {
    fn feed(&mut self, samples: &[i16]) -> Result<Vec<Segment>, String>;
    fn finish(&mut self) -> Vec<Segment>;
    /// Total samples fed since construction.
    fn all_len(&self) -> usize;
    /// Slice retained global samples by absolute indices.
    fn sample_slice(&self, start_sample: usize, end_sample: usize) -> Vec<i16>;
    /// Drop retained samples before `sample_idx` (absolute). Only safe to call
    /// once no pending/future group needs earlier samples.
    fn prune_before(&mut self, _sample_idx: usize) {}
}

pub struct IncrementalVadSource(pub IncrementalVad);

impl IncrementalVadSource {
    pub fn new(sample_rate: u32) -> Result<Self, String> {
        IncrementalVad::new(sample_rate)
            .map(Self)
            .map_err(|e| e.to_string())
    }
}

impl VadSource for IncrementalVadSource {
    fn feed(&mut self, samples: &[i16]) -> Result<Vec<Segment>, String> {
        self.0.feed(samples).map_err(|e| e.to_string())
    }

    fn finish(&mut self) -> Vec<Segment> {
        self.0.finish()
    }

    fn all_len(&self) -> usize {
        self.0.all_samples_len()
    }

    fn sample_slice(&self, start_sample: usize, end_sample: usize) -> Vec<i16> {
        self.0.sample_slice(start_sample, end_sample)
    }

    fn prune_before(&mut self, sample_idx: usize) {
        self.0.prune_before(sample_idx);
    }
}

/// Streams audio through a VAD and groups the resulting speech segments with
/// the same Min-Cut + Merge algorithm as the file re-transcribe path
/// (`min_cut_merge::min_cut_merge`), emitting `AudioSegment`s whose samples
/// span [first speech start, last speech end] (intra-group silence included).
///
/// Grouping decisions depend only on the ordered VAD segment list, so feeding
/// audio incrementally produces the identical groups as batch-processing the
/// full recording.
pub struct VadGroupStream {
    vad: Box<dyn VadSource>,
    sample_rate: u32,
    max_dur_s: f64,
    pending: Vec<Segment>,
    pending_dur: f64,
    next_index: usize,
    emitted_any: bool,
    fed_samples: usize,
    collect_vad_segments: bool,
    vad_segments: Vec<Segment>,
}

impl VadGroupStream {
    pub fn new(config: VadGroupStreamConfig) -> Result<Self, String> {
        let vad = IncrementalVadSource::new(config.sample_rate)?;
        Ok(Self::with_vad(config, Box::new(vad)))
    }

    fn with_vad(config: VadGroupStreamConfig, vad: Box<dyn VadSource>) -> Self {
        Self {
            vad,
            sample_rate: config.sample_rate,
            max_dur_s: config.max_duration_ms as f64 / 1000.0,
            pending: Vec::new(),
            pending_dur: 0.0,
            next_index: 0,
            emitted_any: false,
            fed_samples: 0,
            collect_vad_segments: config.collect_vad_segments,
            vad_segments: Vec::new(),
        }
    }

    fn push_vad_segment(&mut self, seg: Segment, out: &mut Vec<AudioSegment>) {
        let atoms = if seg.end - seg.start > self.max_dur_s + 0.01 {
            split_segment_at_energy(&seg.samples, seg.start, self.sample_rate, self.max_dur_s)
        } else {
            vec![seg]
        };
        for atom in atoms {
            self.merge_atom(atom, out);
        }
    }

    /// Mirrors `merge_segments` from `min_cut_merge.rs` decision-for-decision.
    fn merge_atom(&mut self, atom: Segment, out: &mut Vec<AudioSegment>) {
        let seg_dur = atom.end - atom.start;
        let would_exceed = self.pending_dur + seg_dur > self.max_dur_s;

        if !would_exceed {
            self.pending.push(atom);
            self.pending_dur += seg_dur;
        } else if self.pending_dur >= self.max_dur_s * 0.8 {
            self.emit_group(out);
            self.pending.push(atom);
            self.pending_dur = seg_dur;
        } else {
            if !self.pending.is_empty() {
                self.emit_group(out);
            }
            self.pending.push(atom);
            self.pending_dur = seg_dur;
        }
    }

    fn emit_group(&mut self, out: &mut Vec<AudioSegment>) {
        if self.pending.is_empty() {
            return;
        }
        let first = &self.pending[0];
        let last = self.pending.last().unwrap();
        let start_sample = (first.start * self.sample_rate as f64) as usize;
        let end_sample = ((last.end * self.sample_rate as f64) as usize).min(self.vad.all_len());
        let samples_i16 = self.vad.sample_slice(start_sample, end_sample);
        out.push(AudioSegment {
            index: self.next_index,
            global_start_ms: (first.start * 1000.0) as i64,
            global_end_ms: (last.end * 1000.0) as i64,
            samples: samples_i16.iter().map(|&s| s as f32 / 32767.0).collect(),
        });
        self.next_index += 1;
        self.emitted_any = true;
        self.pending.clear();
        self.pending_dur = 0.0;
        // The next group's first speech starts at or after `end_sample`, so
        // samples before it are no longer needed. Pruning keeps the retained
        // buffer bounded instead of growing to the full recording length.
        self.vad.prune_before(end_sample);
    }
}

impl SegmentProducer for VadGroupStream {
    fn feed(&mut self, samples: &[f32]) -> Vec<AudioSegment> {
        let i16: Vec<i16> = samples
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        self.fed_samples += i16.len();
        let segments = match self.vad.feed(&i16) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "[vad-group] VAD feed failed");
                return Vec::new();
            }
        };
        if segments.is_empty() {
            return Vec::new();
        }
        if self.collect_vad_segments {
            self.vad_segments.extend(segments.iter().cloned());
        }
        let mut out = Vec::new();
        for seg in segments {
            self.push_vad_segment(seg, &mut out);
        }
        out
    }

    fn flush(&mut self) -> Vec<AudioSegment> {
        let mut out = Vec::new();
        let finished = self.vad.finish();
        if self.collect_vad_segments {
            self.vad_segments.extend(finished.iter().cloned());
        }
        for seg in finished {
            self.push_vad_segment(seg, &mut out);
        }
        if !self.pending.is_empty() {
            self.emit_group(&mut out);
        }
        // No speech detected at all → fall back to submitting the whole
        // recording as one group (mirrors `submit_file_direct` for empty VAD).
        if !self.emitted_any && self.fed_samples > 0 {
            let end_sample = self.fed_samples.min(self.vad.all_len());
            let samples_i16 = self.vad.sample_slice(0, end_sample);
            out.push(AudioSegment {
                index: self.next_index,
                global_start_ms: 0,
                global_end_ms: (self.fed_samples as f64 / self.sample_rate as f64 * 1000.0) as i64,
                samples: samples_i16.iter().map(|&s| s as f32 / 32767.0).collect(),
            });
            self.next_index += 1;
        }
        out
    }

    fn take_vad_segments(&mut self) -> Vec<Segment> {
        std::mem::take(&mut self.vad_segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypr_pyannote_local::min_cut_merge::{MinCutMergeConfig, min_cut_merge};
    use std::sync::{Arc, Mutex};

    struct MockVad {
        feed_calls: Arc<Mutex<Vec<Vec<Segment>>>>,
        pending_segments: Vec<Segment>,
        total: usize,
    }

    impl MockVad {
        fn new(segments: Vec<Segment>) -> Self {
            Self {
                feed_calls: Arc::new(Mutex::new(Vec::new())),
                pending_segments: segments,
                total: 0,
            }
        }

        fn seg(start: f64, end: f64, sr: u32) -> Segment {
            let len = ((end - start) * sr as f64) as usize;
            let mut samples = vec![100i16; len];
            let mid = len / 2;
            if len > 0 {
                samples[mid] = 0;
            }
            Segment {
                start,
                end,
                samples,
            }
        }
    }

    impl VadSource for MockVad {
        fn feed(&mut self, samples: &[i16]) -> Result<Vec<Segment>, String> {
            self.total += samples.len();
            self.feed_calls.lock().unwrap().push(Vec::new());
            let mut out = Vec::new();
            // Only emit segments whose start is within already-fed samples.
            let fed_s = self.total as f64 / 16000.0;
            let idx = self.pending_segments.partition_point(|s| s.start < fed_s);
            out.extend(self.pending_segments.drain(..idx));
            Ok(out)
        }

        fn finish(&mut self) -> Vec<Segment> {
            std::mem::take(&mut self.pending_segments)
        }

        fn all_len(&self) -> usize {
            // Give the mock enough samples to cover the last pending segment.
            let last_end = self
                .pending_segments
                .last()
                .map(|s| (s.end * 16000.0) as usize)
                .unwrap_or(self.total);
            self.total.max(last_end)
        }

        fn sample_slice(&self, start_sample: usize, end_sample: usize) -> Vec<i16> {
            let len = end_sample.saturating_sub(start_sample);
            vec![100i16; len]
        }
    }

    fn stream(max_dur_ms: u32, vad: MockVad) -> VadGroupStream {
        VadGroupStream::with_vad(
            VadGroupStreamConfig {
                sample_rate: 16000,
                max_duration_ms: max_dur_ms,
                collect_vad_segments: false,
            },
            Box::new(vad),
        )
    }

    #[test]
    fn test_empty_input_produces_no_groups() {
        let mut s = stream(30000, MockVad::new(vec![]));
        assert!(s.feed(&[]).is_empty());
        assert!(s.flush().is_empty());
    }

    #[test]
    fn test_groups_match_batch_min_cut_merge() {
        // Same VAD segment list as the file path would see.
        let sr = 16000;
        let segments = vec![
            MockVad::seg(0.0, 10.0, sr),
            MockVad::seg(12.0, 22.0, sr),
            MockVad::seg(24.0, 36.0, sr),
            MockVad::seg(40.0, 60.0, sr),
        ];
        let max_dur_ms = 30000;

        // Batch reference.
        let merged = min_cut_merge(
            segments.clone(),
            &[],
            sr,
            MinCutMergeConfig {
                max_duration_ms: max_dur_ms,
            },
        );

        // Incremental: feed in chunks; MockVad emits segments as fed.
        let mut s = stream(max_dur_ms, MockVad::new(segments));
        let mut emitted = Vec::new();
        // 10s chunks cover all segments by t=60s.
        for _ in 0..12 {
            emitted.extend(s.feed(&vec![0.0f32; (sr * 5) as usize]));
        }
        emitted.extend(s.flush());

        let batch_bounds: Vec<(f64, f64)> = merged
            .iter()
            .map(|g| (g[0].start, g.last().unwrap().end))
            .collect();
        let stream_bounds: Vec<(i64, i64)> = emitted
            .iter()
            .map(|g| (g.global_start_ms, g.global_end_ms))
            .collect();

        assert_eq!(
            stream_bounds.len(),
            batch_bounds.len(),
            "group count must match batch min_cut_merge"
        );
        for (i, (sb, bb)) in stream_bounds.iter().zip(batch_bounds.iter()).enumerate() {
            assert!(
                (sb.0 as f64 / 1000.0 - bb.0).abs() < 0.01,
                "group {i} start mismatch: stream {} vs batch {:?}",
                sb.0,
                bb
            );
            assert!(
                (sb.1 as f64 / 1000.0 - bb.1).abs() < 0.01,
                "group {i} end mismatch: stream {} vs batch {:?}",
                sb.1,
                bb
            );
        }
    }

    #[test]
    fn test_incremental_feed_matches_single_feed() {
        let sr = 16000;
        let segs_a = vec![
            MockVad::seg(0.0, 8.0, sr),
            MockVad::seg(10.0, 18.0, sr),
            MockVad::seg(20.0, 32.0, sr),
        ];
        let segs_b = segs_a.clone();

        // Feed all at once.
        let mut s1 = stream(30000, MockVad::new(segs_a));
        let mut one_shot = s1.feed(&vec![0.0f32; (sr * 32) as usize]);
        one_shot.extend(s1.flush());

        // Feed incrementally.
        let mut s2 = stream(30000, MockVad::new(segs_b));
        let mut incremental = Vec::new();
        for _ in 0..32 {
            incremental.extend(s2.feed(&vec![0.0f32; sr as usize]));
        }
        incremental.extend(s2.flush());

        let bounds1: Vec<(i64, i64)> = one_shot
            .iter()
            .map(|g| (g.global_start_ms, g.global_end_ms))
            .collect();
        let bounds2: Vec<(i64, i64)> = incremental
            .iter()
            .map(|g| (g.global_start_ms, g.global_end_ms))
            .collect();
        assert_eq!(bounds1, bounds2);
    }

    #[test]
    fn test_long_segment_is_min_cut_split() {
        // 60s segment with max 30s → split into two groups.
        let sr = 16000;
        let mut s = stream(30000, MockVad::new(vec![MockVad::seg(0.0, 60.0, sr)]));
        let mut emitted = s.feed(&vec![0.0f32; (sr * 60) as usize]);
        emitted.extend(s.flush());
        assert!(
            emitted.len() >= 2,
            "60s segment should split: {:?}",
            emitted
        );
        for g in &emitted {
            let dur_s = (g.global_end_ms - g.global_start_ms) as f64 / 1000.0;
            assert!(
                dur_s <= 30.0 + 0.5,
                "each split group <= max_duration (+tolerance), got {dur_s:.1}s"
            );
        }
    }

    #[test]
    fn test_flush_emits_pending_group() {
        let sr = 16000;
        // A 5s segment alone is below max → held pending until flush.
        let mut s = stream(30000, MockVad::new(vec![MockVad::seg(0.0, 5.0, sr)]));
        assert!(s.feed(&vec![0.0f32; (sr * 5) as usize]).is_empty());
        let flushed = s.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].index, 0);
        assert_eq!(flushed[0].global_start_ms, 0);
        assert_eq!(flushed[0].global_end_ms, 5000);
    }

    #[test]
    fn test_no_speech_falls_back_to_whole_recording() {
        // MockVad never emits any segment → flush emits the whole audio.
        let mut s = stream(30000, MockVad::new(vec![]));
        assert!(s.feed(&vec![0.0f32; 16000]).is_empty());
        let flushed = s.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].index, 0);
        assert_eq!(flushed[0].global_start_ms, 0);
        assert_eq!(flushed[0].global_end_ms, 1000);
        assert_eq!(flushed[0].samples.len(), 16000);
    }

    #[test]
    fn test_group_span_includes_intra_group_silence() {
        let sr = 16000;
        // Two 5s segments with a 10s gap merge into one group whose samples
        // span the full [0, 20] window (silence included).
        let mut s = stream(
            30000,
            MockVad::new(vec![
                MockVad::seg(0.0, 5.0, sr),
                MockVad::seg(15.0, 20.0, sr),
            ]),
        );
        let mut emitted = s.feed(&vec![0.0f32; (sr * 20) as usize]);
        emitted.extend(s.flush());
        assert_eq!(emitted.len(), 1, "both segments merge into one group");
        assert_eq!(emitted[0].global_start_ms, 0);
        assert_eq!(emitted[0].global_end_ms, 20000);
        assert_eq!(emitted[0].samples.len(), (sr * 20) as usize);
    }

    #[test]
    fn test_take_vad_segments_returns_emitted_segments_when_collecting() {
        let sr = 16000;
        let mut s = VadGroupStream::with_vad(
            VadGroupStreamConfig {
                sample_rate: sr,
                max_duration_ms: 30000,
                collect_vad_segments: true,
            },
            Box::new(MockVad::new(vec![
                MockVad::seg(0.0, 5.0, sr),
                MockVad::seg(10.0, 15.0, sr),
            ])),
        );
        let _ = s.feed(&vec![0.0f32; (sr * 20) as usize]);
        let taken = s.take_vad_segments();
        assert_eq!(taken.len(), 2, "both VAD segments collected");
        assert_eq!(taken[0].start, 0.0);
        assert_eq!(taken[1].start, 10.0);
        assert!(s.take_vad_segments().is_empty(), "take drains the buffer");
    }

    #[test]
    fn test_take_vad_segments_empty_when_not_collecting() {
        let sr = 16000;
        let mut s = VadGroupStream::with_vad(
            VadGroupStreamConfig {
                sample_rate: sr,
                max_duration_ms: 30000,
                collect_vad_segments: false,
            },
            Box::new(MockVad::new(vec![MockVad::seg(0.0, 5.0, sr)])),
        );
        let _ = s.feed(&vec![0.0f32; (sr * 10) as usize]);
        assert!(s.take_vad_segments().is_empty());
    }
}
