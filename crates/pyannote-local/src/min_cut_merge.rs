use crate::diarization::SpeakerSegment;
use crate::segmentation::Segment;

#[derive(Debug, Clone)]
pub struct MinCutMergeConfig {
    pub max_duration_ms: u32,
}

impl Default for MinCutMergeConfig {
    fn default() -> Self {
        Self {
            max_duration_ms: 30000,
        }
    }
}

const RMS_WINDOW: usize = 160;

/// Apply Min-Cut (split long segments at lowest-energy point) then Merge
/// (combine short segments up to max_duration).
///
/// Operates on VAD `Segment`s that carry their own `samples`.
pub fn min_cut_merge(
    segments: Vec<Segment>,
    _pcm_i16: &[i16],
    sample_rate: u32,
    config: MinCutMergeConfig,
) -> Vec<Vec<Segment>> {
    let max_dur_s = config.max_duration_ms as f64 / 1000.0;

    let mut cut = Vec::new();
    for seg in segments {
        let dur = seg.end - seg.start;
        if dur > max_dur_s + 0.01 {
            let splits = split_segment_at_energy(&seg.samples, seg.start, sample_rate, max_dur_s);
            cut.extend(splits);
        } else {
            cut.push(seg);
        }
    }

    merge_segments(cut, max_dur_s)
}

/// Version of Min-Cut + Merge that works on [`SpeakerSegment`] by
/// computing RMS energy from the global PCM buffer.
///
/// Each split produces two `SpeakerSegment`s with the same `speaker` and
/// `embedding_valid` as the original.
pub fn min_cut_merge_speaker(
    segments: Vec<SpeakerSegment>,
    audio_f32: &[f32],
    sample_rate: u32,
    config: MinCutMergeConfig,
) -> Vec<Vec<SpeakerSegment>> {
    fn seg_to_samples(seg: &SpeakerSegment, audio: &[f32], sr: u32) -> Vec<i16> {
        let lo = (seg.start * sr as f64) as usize;
        let hi = ((seg.end * sr as f64) as usize).min(audio.len());
        audio[lo..hi]
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect()
    }

    let max_dur_s = config.max_duration_ms as f64 / 1000.0;

    let mut cut: Vec<(SpeakerSegment, Vec<i16>)> = Vec::new();
    for seg in segments {
        let dur = seg.end - seg.start;
        if dur > max_dur_s + 0.01 {
            let samples = seg_to_samples(&seg, audio_f32, sample_rate);
            let splits = split_segment_at_energy(&samples, seg.start, sample_rate, max_dur_s);
            for s in splits {
                cut.push(seg_with_speaker(&s, seg.speaker, seg.embedding_valid));
            }
        } else {
            let samples = seg_to_samples(&seg, audio_f32, sample_rate);
            cut.push((seg, samples));
        }
    }

    // Re-merge into groups without splitting further
    merge_speaker_groups(cut, max_dur_s)
}

fn seg_with_speaker(
    seg: &Segment,
    speaker: usize,
    embedding_valid: bool,
) -> (SpeakerSegment, Vec<i16>) {
    (
        SpeakerSegment {
            start: seg.start,
            end: seg.end,
            speaker,
            embedding_valid,
        },
        seg.samples.clone(),
    )
}

/// Split a long VAD segment into turn-aligned sub-chunks using short-term
/// energy pauses.
///
/// Cut points are chosen at the midpoint of each silence valley longer than
/// `min_pause_s` — i.e. determined by the audio's own structure rather than a
/// fixed window. Segments (or stretches) with no detectable pause are split at
/// `max_chunk_s` intervals as a fallback so a long run of continuous speech
/// still yields multiple embeddings.
///
/// Returns chunk (start_s, end_s) pairs relative to `global_start`.
pub fn split_into_turn_chunks(
    samples: &[i16],
    global_start: f64,
    sample_rate: u32,
    min_pause_s: f64,
    max_chunk_s: f64,
) -> Vec<(f64, f64)> {
    let dur_s = samples.len() as f64 / sample_rate as f64;
    if dur_s <= max_chunk_s + 0.01 {
        return vec![(global_start, global_start + dur_s)];
    }

    let rms_win = (0.03 * sample_rate as f64) as usize;
    let rms_hop = rms_win;
    let mut profile: Vec<f64> = Vec::new();
    let mut i = 0;
    while i < samples.len() {
        let end = (i + rms_win).min(samples.len());
        let frame = &samples[i..end];
        let sum = frame
            .iter()
            .map(|&s| (s as f64 / 32768.0).powi(2))
            .sum::<f64>()
            / frame.len() as f64;
        profile.push(sum.sqrt());
        i += rms_hop;
    }

    // Adaptive threshold from the noise floor (10th percentile). Capped at half
    // the peak RMS so a constant-amplitude signal (no real dynamic range) is
    // never classified entirely as pause.
    let mut sorted = profile.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let noise_floor = sorted[sorted.len() / 10];
    let median = sorted[sorted.len() / 2];
    let peak = *sorted.last().unwrap();
    let threshold = (noise_floor * 3.0).max(median * 0.3).min(peak * 0.5);

    // Find silence valleys: consecutive RMS frames below threshold.
    let mut pauses: Vec<(usize, usize)> = Vec::new();
    let mut in_pause = false;
    let mut p_start = 0usize;
    for (idx, &r) in profile.iter().enumerate() {
        if r < threshold {
            if !in_pause {
                in_pause = true;
                p_start = idx;
            }
        } else if in_pause {
            in_pause = false;
            pauses.push((p_start, idx));
        }
    }
    if in_pause {
        pauses.push((p_start, profile.len()));
    }

    let min_pause_frames = (min_pause_s / 0.03) as usize;
    let max_chunk_frames = (max_chunk_s / 0.03) as usize;

    let mut cuts: Vec<f64> = Vec::new();
    let mut last = 0.0f64;
    let dur_frames = profile.len() as f64 * 0.03;

    for (a, b) in pauses.iter().filter(|(a, b)| b - a >= min_pause_frames) {
        let mid = (a + b) as f64 / 2.0 * 0.03;
        if mid - last >= 1.0 && dur_frames - mid >= 1.0 {
            cuts.push(mid);
            last = mid;
        }
    }

    // Fallback: cap any stretch without a pause cut.
    let mut final_cuts: Vec<f64> = Vec::new();
    let mut anchor = 0.0f64;
    for &c in &cuts {
        while c - anchor > max_chunk_frames as f64 * 0.03 + 0.01 {
            anchor += max_chunk_frames as f64 * 0.03;
            final_cuts.push(anchor);
        }
        final_cuts.push(c);
        anchor = c;
    }
    while dur_frames - anchor > max_chunk_frames as f64 * 0.03 + 0.01 {
        anchor += max_chunk_frames as f64 * 0.03;
        final_cuts.push(anchor);
    }

    let mut pts = vec![0.0];
    pts.extend(final_cuts);
    pts.push(dur_s);
    pts.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    pts.iter()
        .zip(pts.iter().skip(1))
        .map(|(&a, &b)| (global_start + a, global_start + b))
        .filter(|(a, b)| b - a >= 0.05)
        .collect()
}

/// Split segment samples at lowest-RMS point in [½τ, τ].
pub fn split_segment_at_energy(
    samples: &[i16],
    global_start: f64,
    sample_rate: u32,
    max_dur_s: f64,
) -> Vec<Segment> {
    let half_tau = (max_dur_s / 2.0 * sample_rate as f64) as usize;
    let tau = (max_dur_s * sample_rate as f64) as usize;

    let search_lo = half_tau.min(samples.len().saturating_sub(RMS_WINDOW));
    let search_hi = tau.min(samples.len().saturating_sub(RMS_WINDOW));

    let split_pos = if search_lo >= search_hi || search_lo >= samples.len() {
        samples.len() / 2
    } else {
        let mut min_rms = f64::MAX;
        let mut min_pos = search_lo;
        for pos in (search_lo..=search_hi).step_by(RMS_WINDOW) {
            let end = (pos + RMS_WINDOW).min(samples.len());
            let frame = &samples[pos..end];
            let rms = frame
                .iter()
                .map(|&s| (s as f64 / 32768.0).powi(2))
                .sum::<f64>()
                / frame.len() as f64;
            if rms < min_rms {
                min_rms = rms;
                min_pos = pos;
            }
        }
        min_pos
    };

    if split_pos == 0 || split_pos >= samples.len() {
        return vec![Segment {
            start: global_start,
            end: global_start + samples.len() as f64 / sample_rate as f64,
            samples: samples.to_vec(),
        }];
    }

    let split_time = global_start + split_pos as f64 / sample_rate as f64;
    vec![
        Segment {
            start: global_start,
            end: split_time,
            samples: samples[..split_pos].to_vec(),
        },
        Segment {
            start: split_time,
            end: global_start + samples.len() as f64 / sample_rate as f64,
            samples: samples[split_pos..].to_vec(),
        },
    ]
}

fn merge_segments(segments: Vec<Segment>, max_dur_s: f64) -> Vec<Vec<Segment>> {
    let mut groups: Vec<Vec<Segment>> = Vec::new();
    let mut pending: Vec<Segment> = Vec::new();
    let mut pending_dur = 0.0;

    for seg in segments {
        let seg_dur = seg.end - seg.start;

        let would_exceed = pending_dur + seg_dur > max_dur_s;

        if !would_exceed {
            pending.push(seg);
            pending_dur += seg_dur;
        } else if pending_dur >= max_dur_s * 0.8 {
            groups.push(std::mem::take(&mut pending));
            pending.push(seg);
            pending_dur = seg_dur;
        } else {
            if !pending.is_empty() {
                groups.push(std::mem::take(&mut pending));
            }
            pending.push(seg);
            pending_dur = seg_dur;
        }
    }

    if !pending.is_empty() {
        groups.push(pending);
    }

    groups
}

fn merge_speaker_groups(
    segments: Vec<(SpeakerSegment, Vec<i16>)>,
    max_dur_s: f64,
) -> Vec<Vec<SpeakerSegment>> {
    let mut groups: Vec<Vec<SpeakerSegment>> = Vec::new();
    let mut pending: Vec<SpeakerSegment> = Vec::new();
    let mut pending_dur = 0.0;

    for (seg, _samples) in segments {
        let seg_dur = seg.end - seg.start;
        let would_exceed = pending_dur + seg_dur > max_dur_s;

        if !would_exceed {
            pending.push(seg);
            pending_dur += seg_dur;
        } else if pending_dur >= max_dur_s * 0.8 {
            groups.push(std::mem::take(&mut pending));
            pending.push(seg);
            pending_dur = seg_dur;
        } else {
            if !pending.is_empty() {
                groups.push(std::mem::take(&mut pending));
            }
            pending.push(seg);
            pending_dur = seg_dur;
        }
    }

    if !pending.is_empty() {
        groups.push(pending);
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64) -> Segment {
        let sr: u32 = 16000;
        let len = ((end - start) * sr as f64) as usize;
        Segment {
            start,
            end,
            samples: vec![0i16; len],
        }
    }

    fn seg_with_noise(start: f64, end: f64, silence_start: f64, silence_end: f64) -> Segment {
        let sr: u32 = 16000;
        let len = ((end - start) * sr as f64) as usize;
        let mut samples = vec![100i16; len];
        let si_lo = ((silence_start - start) * sr as f64) as usize;
        let si_hi = ((silence_end - start) * sr as f64) as usize;
        for i in si_lo..si_hi.min(len) {
            samples[i] = 0;
        }
        Segment {
            start,
            end,
            samples,
        }
    }

    #[test]
    fn test_short_segments_single_group() {
        let result = min_cut_merge(
            vec![seg(0.0, 10.0), seg(10.0, 20.0)],
            &[],
            16000,
            MinCutMergeConfig::default(),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 2);
    }

    #[test]
    fn test_merge_up_to_threshold() {
        let result = min_cut_merge(
            vec![seg(0.0, 12.0), seg(12.0, 24.0), seg(24.0, 36.0)],
            &[],
            16000,
            MinCutMergeConfig::default(),
        );
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 2);
        assert_eq!(result[1].len(), 1);
    }

    #[test]
    fn test_min_cut_long_segment() {
        let seg = seg_with_noise(0.0, 45.0, 27.0, 28.0);
        let result = min_cut_merge(vec![seg], &[], 16000, MinCutMergeConfig::default());
        assert_eq!(result.len(), 2);
        let total_samples: usize = result
            .iter()
            .flat_map(|g| g.iter())
            .map(|s| s.samples.len())
            .sum();
        assert_eq!(total_samples, 45 * 16000);
    }

    #[test]
    fn test_empty_input() {
        let result = min_cut_merge(vec![], &[], 16000, MinCutMergeConfig::default());
        assert!(result.is_empty());
    }

    #[test]
    fn test_turn_chunks_single_short_segment() {
        let sr: u32 = 16000;
        let samples = vec![100i16; sr as usize * 2];
        let chunks = split_into_turn_chunks(&samples, 10.0, sr, 0.2, 4.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], (10.0, 12.0));
    }

    #[test]
    fn test_turn_chunks_no_pause_fallback_cap() {
        let sr: u32 = 16000;
        let samples = vec![100i16; sr as usize * 10];
        let chunks = split_into_turn_chunks(&samples, 0.0, sr, 0.2, 4.0);
        // No pauses (constant 100 amplitude → no valley below threshold).
        // 10s / 4s cap → ceil(10/4) = 3 chunks.
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|&(a, b)| b > a));
        assert_eq!(chunks[0].0, 0.0);
        assert!((chunks.last().unwrap().1 - 10.0).abs() < 0.1);
    }

    #[test]
    fn test_turn_chunks_split_at_silence_valley() {
        let sr: u32 = 16000;
        // 2s loud, 0.4s silence, 2s loud → split at pause midpoint (~2.2s)
        let mut samples = vec![100i16; sr as usize * 2];
        samples.extend(vec![0i16; (0.4 * sr as f64) as usize]);
        samples.extend(vec![100i16; sr as usize * 2]);
        let chunks = split_into_turn_chunks(&samples, 0.0, sr, 0.2, 4.0);
        assert_eq!(chunks.len(), 2);
        assert!(
            (chunks[0].1 - 2.2).abs() < 0.2,
            "first ends near pause midpoint ~2.2s, got {}",
            chunks[0].1
        );
        assert_eq!(chunks[0].1, chunks[1].0);
    }

    #[test]
    fn test_turn_chunks_silence_too_short_ignored() {
        let sr: u32 = 16000;
        // 1.5s loud, 0.1s silence (below min_pause), 1.5s loud → single chunk (≤ cap)
        let mut samples = vec![100i16; (1.5 * sr as f64) as usize];
        samples.extend(vec![0i16; (0.1 * sr as f64) as usize]);
        samples.extend(vec![100i16; (1.5 * sr as f64) as usize]);
        let chunks = split_into_turn_chunks(&samples, 0.0, sr, 0.2, 4.0);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_single_short_segment() {
        let result = min_cut_merge(
            vec![seg(0.0, 5.0)],
            &[],
            16000,
            MinCutMergeConfig::default(),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 1);
    }

    #[test]
    fn test_exact_max_duration_no_split() {
        let result = min_cut_merge(
            vec![seg(0.0, 30.0)],
            &[],
            16000,
            MinCutMergeConfig::default(),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 1);
    }

    #[test]
    fn test_merge_overflow_flush_partial() {
        let result = min_cut_merge(
            vec![seg(0.0, 3.0), seg(3.0, 6.0), seg(6.0, 45.0)],
            &[],
            16000,
            MinCutMergeConfig::default(),
        );
        // After Min-Cut: 0-3, 3-6, 6-21 (search_lo = 15s), 21-45
        // Merge: [0-3, 3-6, 6-21] (21s < 80%, pushed on overflow), [21-45]
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 3);
        assert_eq!(result[1].len(), 1);
    }
}
