use std::cmp;

#[derive(Debug, Clone)]
pub struct SegmenterConfig {
    pub sample_rate: u32,
    pub segment_duration_ms: u32,
    pub overlap_ms: u32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            segment_duration_ms: 30000,
            overlap_ms: 1000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioSegment {
    pub index: usize,
    pub global_start_ms: i64,
    pub global_end_ms: i64,
    pub samples: Vec<f32>,
}

#[derive(Debug)]
pub struct Segmenter {
    config: SegmenterConfig,
    segment_samples: usize,
    overlap_samples: usize,
    /// Stride samples between consecutive segment starts = segment_samples - overlap_samples.
    /// Each iteration of the feed loop consumes this many fresh samples from the buffer.
    stride: usize,
    overlap_ring: Vec<f32>,
    buffer: Vec<f32>,
    total_samples: u64,
    next_index: usize,
}

impl Segmenter {
    pub fn new(config: SegmenterConfig) -> Self {
        assert!(config.sample_rate > 0, "sample_rate must be > 0");
        assert!(
            config.segment_duration_ms > 0,
            "segment_duration_ms must be > 0"
        );

        let segment_samples =
            (config.segment_duration_ms as f64 / 1000.0 * config.sample_rate as f64) as usize;
        let overlap_samples =
            (config.overlap_ms as f64 / 1000.0 * config.sample_rate as f64) as usize;
        let overlap_samples = cmp::min(overlap_samples, segment_samples.saturating_sub(1));
        let stride = segment_samples - overlap_samples;

        Self {
            segment_samples,
            overlap_samples,
            stride,
            overlap_ring: Vec::new(),
            buffer: Vec::new(),
            total_samples: 0,
            next_index: 0,
            config,
        }
    }

    pub fn feed(&mut self, samples: &[f32]) -> Vec<AudioSegment> {
        self.buffer.extend_from_slice(samples);
        self.total_samples += samples.len() as u64;

        let max_iterations = if self.stride > 0 {
            self.buffer.len() / self.stride + 2
        } else {
            1
        };

        let mut segments = Vec::new();
        for _ in 0..max_iterations {
            let available = self.buffer.len() + self.overlap_ring.len();
            if available < self.segment_samples {
                break;
            }

            let take_from_overlap = cmp::min(self.overlap_ring.len(), self.segment_samples);
            let take_from_buffer = self.segment_samples - take_from_overlap;

            let mut segment = Vec::with_capacity(self.segment_samples);
            segment.extend_from_slice(&self.overlap_ring);
            segment.extend_from_slice(&self.buffer[..take_from_buffer]);

            self.overlap_ring = segment[self.segment_samples - self.overlap_samples..].to_vec();
            self.buffer.drain(..take_from_buffer);

            let start_ms = self.segment_start_ms(self.next_index);
            let end_ms = start_ms + self.config.segment_duration_ms as i64;

            segments.push(AudioSegment {
                index: self.next_index,
                global_start_ms: start_ms,
                global_end_ms: end_ms,
                samples: segment,
            });
            self.next_index += 1;
        }

        if !segments.is_empty() {
            tracing::debug!(
                "[progressive] segmenter feed produced {} segments (idx {}-{}, buffer_len={}, total_samples={})",
                segments.len(),
                segments.first().map(|s| s.index).unwrap_or(0),
                segments.last().map(|s| s.index).unwrap_or(0),
                self.buffer.len(),
                self.total_samples,
            );
        }

        segments
    }

    pub fn flush(&mut self) -> Vec<AudioSegment> {
        if self.overlap_ring.is_empty() && self.buffer.is_empty() {
            return Vec::new();
        }

        let total_duration = self.total_duration_ms();
        let remaining_duration = ((self.overlap_ring.len() + self.buffer.len()) as f64
            / self.config.sample_rate as f64
            * 1000.0) as i64;
        let start_ms = total_duration - remaining_duration;

        let mut segment = std::mem::take(&mut self.overlap_ring);
        segment.append(&mut self.buffer);

        let seg = AudioSegment {
            index: self.next_index,
            global_start_ms: start_ms,
            global_end_ms: total_duration,
            samples: segment,
        };
        self.next_index += 1;

        vec![seg]
    }

    pub fn total_duration_ms(&self) -> i64 {
        (self.total_samples as f64 / self.config.sample_rate as f64 * 1000.0) as i64
    }

    fn segment_start_ms(&self, index: usize) -> i64 {
        let stride_s = self
            .config
            .segment_duration_ms
            .saturating_sub(self.config.overlap_ms) as f64
            / 1000.0;
        (index as f64 * stride_s * 1000.0) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_custom(sr: u32, seg_ms: u32, overlap_ms: u32) -> SegmenterConfig {
        SegmenterConfig {
            sample_rate: sr,
            segment_duration_ms: seg_ms,
            overlap_ms,
        }
    }

    #[test]
    fn test_empty_feed_produces_no_segments() {
        let mut seg = Segmenter::new(SegmenterConfig::default());
        assert!(seg.feed(&[]).is_empty());
    }

    #[test]
    fn test_less_than_one_segment_returns_nothing() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        assert!(seg.feed(&[0.0; 50]).is_empty());
        assert_eq!(seg.total_duration_ms(), 50);
    }

    #[test]
    fn test_exactly_one_segment() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let segments = seg.feed(&[0.5; 100]);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].index, 0);
        assert_eq!(segments[0].global_start_ms, 0);
        assert_eq!(segments[0].global_end_ms, 100);
        assert_eq!(segments[0].samples.len(), 100);
    }

    #[test]
    fn test_two_segments_with_overlap() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let samples: Vec<f32> = (0..190).map(|i| i as f32).collect();
        let segments = seg.feed(&samples);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].index, 0);
        assert_eq!(segments[0].global_start_ms, 0);
        assert_eq!(segments[0].global_end_ms, 100);
        assert_eq!(segments[0].samples.len(), 100);
        assert_eq!(
            segments[0].samples[..10],
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]
        );

        assert_eq!(segments[1].index, 1);
        assert_eq!(segments[1].global_start_ms, 90);
        assert_eq!(segments[1].global_end_ms, 190);
        assert_eq!(segments[1].samples.len(), 100);
        assert_eq!(
            segments[1].samples[..10],
            [90.0, 91.0, 92.0, 93.0, 94.0, 95.0, 96.0, 97.0, 98.0, 99.0]
        );
    }

    #[test]
    fn test_three_segments_from_single_feed() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let all: Vec<f32> = vec![0.0; 290];
        let segments = seg.feed(&all);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].index, 0);
        assert_eq!(segments[0].global_start_ms, 0);
        assert_eq!(segments[1].index, 1);
        assert_eq!(segments[1].global_start_ms, 90);
        assert_eq!(segments[2].index, 2);
        assert_eq!(segments[2].global_start_ms, 180);
    }

    #[test]
    fn test_flush_remaining_buffer() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        assert_eq!(seg.feed(&[0.0; 100]).len(), 1);

        assert!(seg.feed(&[0.0; 30]).is_empty());

        let flushed = seg.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].index, 1);
        assert_eq!(flushed[0].global_start_ms, 90);
        assert_eq!(flushed[0].global_end_ms, 130);
        assert_eq!(flushed[0].samples.len(), 40);
    }

    #[test]
    fn test_flush_includes_overlap() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let s: Vec<f32> = (0..200).map(|i| i as f32).collect();
        assert_eq!(seg.feed(&s).len(), 2);

        let flushed = seg.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].samples.len(), 20);
        assert_eq!(
            flushed[0].samples,
            [
                180.0, 181.0, 182.0, 183.0, 184.0, 185.0, 186.0, 187.0, 188.0, 189.0, 190.0, 191.0,
                192.0, 193.0, 194.0, 195.0, 196.0, 197.0, 198.0, 199.0
            ]
        );
    }

    #[test]
    fn test_flush_empty_buffer() {
        let mut seg = Segmenter::new(SegmenterConfig::default());
        assert!(seg.flush().is_empty());
    }

    #[test]
    fn test_total_duration_tracking() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        assert_eq!(seg.total_duration_ms(), 0);
        seg.feed(&[0.0; 50]);
        assert_eq!(seg.total_duration_ms(), 50);
        seg.feed(&[0.0; 75]);
        assert_eq!(seg.total_duration_ms(), 125);
    }

    #[test]
    fn test_overlap_clamped_to_segment() {
        let cfg = SegmenterConfig {
            sample_rate: 1000,
            segment_duration_ms: 10,
            overlap_ms: 20,
        };
        let seg = Segmenter::new(cfg);
        assert!(seg.overlap_samples < seg.segment_samples);
    }

    #[test]
    fn test_segment_sample_counts() {
        let cfg = SegmenterConfig {
            sample_rate: 48000,
            segment_duration_ms: 30000,
            overlap_ms: 1000,
        };
        let seg = Segmenter::new(cfg);
        assert_eq!(seg.segment_samples, 1_440_000);
        assert_eq!(seg.overlap_samples, 48_000);
    }

    #[test]
    fn test_overlap_content_from_previous_segment_tail() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let samples: Vec<f32> = (0..200).map(|i| i as f32).collect();
        let s = seg.feed(&samples);
        assert_eq!(s.len(), 2);

        assert_eq!(
            s[0].samples[..10],
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]
        );
        assert_eq!(
            s[0].samples[90..],
            [90.0, 91.0, 92.0, 93.0, 94.0, 95.0, 96.0, 97.0, 98.0, 99.0]
        );

        assert_eq!(s[1].global_start_ms, 90);
        assert_eq!(s[1].global_end_ms, 190);
        assert_eq!(s[1].samples.len(), 100);
        assert_eq!(
            s[1].samples[..10],
            [90.0, 91.0, 92.0, 93.0, 94.0, 95.0, 96.0, 97.0, 98.0, 99.0]
        );
        assert_eq!(
            s[1].samples[90..],
            [
                180.0, 181.0, 182.0, 183.0, 184.0, 185.0, 186.0, 187.0, 188.0, 189.0
            ]
        );
    }

    #[test]
    fn test_multiple_small_feeds_accumulate() {
        let mut seg = Segmenter::new(config_custom(1000, 10, 1));
        assert!(seg.feed(&[0.0; 4]).is_empty());
        assert!(seg.feed(&[0.0; 4]).is_empty());
        let r = seg.feed(&[0.0; 4]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].index, 0);
    }

    #[test]
    fn test_zero_overlap() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 0));
        let s = seg.feed(&vec![0.0; 200]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].global_start_ms, 0);
        assert_eq!(s[0].global_end_ms, 100);
        assert_eq!(s[1].global_start_ms, 100);
        assert_eq!(s[1].global_end_ms, 200);
    }

    #[test]
    fn test_large_overlap_clamped() {
        let mut seg = Segmenter::new(config_custom(1000, 10, 100));
        assert_eq!(seg.overlap_samples, 9);
        assert!(seg.overlap_samples < seg.segment_samples);

        let s = seg.feed(&[0.0; 15]);
        assert_eq!(s.len(), 6);
    }

    #[test]
    fn test_deterministic() {
        let mut seg1 = Segmenter::new(config_custom(1000, 100, 10));
        let mut seg2 = Segmenter::new(config_custom(1000, 100, 10));
        let data: Vec<f32> = (0..250).map(|i| i as f32).collect();

        let r1 = seg1.feed(&data);
        let r2 = seg2.feed(&data);
        assert_eq!(r1.len(), r2.len());
        for (a, b) in r1.iter().zip(r2.iter()) {
            assert_eq!(a.index, b.index);
            assert_eq!(a.global_start_ms, b.global_start_ms);
            assert_eq!(a.global_end_ms, b.global_end_ms);
            assert_eq!(a.samples, b.samples);
        }

        let f1 = seg1.flush();
        let f2 = seg2.flush();
        assert_eq!(f1.len(), f2.len());
    }

    #[test]
    fn test_deterministic_staggered_feeds() {
        let mut seg1 = Segmenter::new(config_custom(1000, 100, 10));
        let mut seg2 = Segmenter::new(config_custom(1000, 100, 10));

        let r1 = seg1.feed(&(0..150).map(|i| i as f32).collect::<Vec<_>>());
        let _r2 = seg2.feed(&(0..75).map(|i| i as f32).collect::<Vec<_>>());
        let r3 = seg2.feed(&(75..150).map(|i| i as f32).collect::<Vec<_>>());

        assert_eq!(r1.len(), r3.len());
        if !r1.is_empty() {
            assert_eq!(r1[0].samples, r3[0].samples);
        }
    }

    #[test]
    fn test_30s_at_48khz() {
        let cfg = SegmenterConfig {
            sample_rate: 48000,
            segment_duration_ms: 30000,
            overlap_ms: 1000,
        };
        let mut seg = Segmenter::new(cfg);
        let samples: Vec<f32> = vec![0.0; 1_440_000];
        let segments = seg.feed(&samples);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].samples.len(), 1_440_000);
        assert_eq!(segments[0].global_start_ms, 0);
        assert_eq!(segments[0].global_end_ms, 30000);
    }

    #[test]
    fn test_realistic_streaming() {
        let mut seg = Segmenter::new(config_custom(16000, 1000, 40));
        let mut total_segments = 0usize;
        for _ in 0..100 {
            let segments = seg.feed(&[0.0; 160]);
            total_segments += segments.len();
        }
        assert_eq!(total_segments, 1);

        let flushed = seg.flush();
        let remaining = flushed.iter().map(|s| s.samples.len()).sum::<usize>();
        assert!(remaining < 16000);
    }

    #[test]
    fn test_overlap_preserves_correct_samples() {
        let mut seg = Segmenter::new(config_custom(1000, 10, 2));
        let samples: Vec<f32> = (0..22).map(|i| i as f32).collect();
        let s = seg.feed(&samples);
        assert_eq!(s.len(), 2);

        assert_eq!(
            s[0].samples,
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]
        );
        assert_eq!(s[1].global_start_ms, 8);
        assert_eq!(s[1].global_end_ms, 18);
        assert_eq!(
            s[1].samples,
            [8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0]
        );
    }

    #[test]
    fn test_continuous_stream_has_no_gaps() {
        let mut seg = Segmenter::new(config_custom(1000, 10, 2));
        let all: Vec<f32> = (0..30).map(|i| i as f32).collect();
        let s = seg.feed(&all);
        assert_eq!(s.len(), 3);

        assert_eq!(
            s[0].samples,
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]
        );
        assert_eq!(
            s[1].samples,
            [8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0]
        );
        assert_eq!(
            s[2].samples,
            [16.0, 17.0, 18.0, 19.0, 20.0, 21.0, 22.0, 23.0, 24.0, 25.0]
        );

        let flushed = seg.flush();
        assert!(!flushed.is_empty());
        assert_eq!(flushed[0].samples, [24.0, 25.0, 26.0, 27.0, 28.0, 29.0]);
    }

    #[test]
    fn test_multiple_flush_idempotent() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        seg.feed(&[0.0; 50]);
        let f1 = seg.flush();
        assert!(!f1.is_empty());
        let f2 = seg.flush();
        assert!(f2.is_empty());
    }

    #[test]
    fn test_large_batch_feed_produces_correct_segment_count() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let samples: Vec<f32> = vec![0.0; 500];
        let segments = seg.feed(&samples);

        let expected = 500 / 90;
        assert_eq!(segments.len(), expected);
    }

    #[test]
    #[should_panic(expected = "sample_rate must be > 0")]
    fn test_zero_sample_rate_panics() {
        Segmenter::new(SegmenterConfig {
            sample_rate: 0,
            segment_duration_ms: 100,
            overlap_ms: 10,
        });
    }

    #[test]
    #[should_panic(expected = "segment_duration_ms must be > 0")]
    fn test_zero_segment_duration_panics() {
        Segmenter::new(SegmenterConfig {
            sample_rate: 1000,
            segment_duration_ms: 0,
            overlap_ms: 10,
        });
    }

    #[test]
    fn test_overlap_greater_than_segment_clamped() {
        let seg = Segmenter::new(config_custom(1000, 10, 20));
        assert_eq!(seg.overlap_samples, 9);
        assert_eq!(seg.stride, 1);
    }

    #[test]
    fn test_stride_is_segment_minus_overlap() {
        let seg = Segmenter::new(config_custom(1000, 100, 10));
        assert_eq!(seg.stride, 90);
    }

    #[test]
    fn test_feed_iteration_bound_does_not_exceed_stride_ratio() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let s = seg.feed(&[0.0; 10_000]);
        let max_possible = 10_000 / 90 + 2;
        assert!(s.len() <= max_possible, "{} > {}", s.len(), max_possible);
    }

    #[test]
    fn test_overlap_ring_is_always_at_most_overlap_samples() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        seg.feed(&[0.0; 500]);
        assert!(seg.overlap_ring.len() <= seg.overlap_samples);
    }

    #[test]
    fn test_overlap_ring_exactly_overlap_samples_after_full_segment() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        seg.feed(&[0.0; 100]);
        assert_eq!(seg.overlap_ring.len(), 10);
    }

    #[test]
    fn test_segment_content_length_always_exact() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let all: Vec<f32> = (0..500).map(|i| i as f32).collect();
        for s in seg.feed(&all) {
            assert_eq!(s.samples.len(), 100);
            assert_eq!(s.global_end_ms - s.global_start_ms, 100);
        }
    }

    #[test]
    fn test_cumulative_covers_all_input_with_overlap() {
        let mut seg = Segmenter::new(config_custom(1000, 10, 2));
        let all: Vec<f32> = (0..50).map(|i| i as f32).collect();
        let produced: Vec<f32> = seg.feed(&all).into_iter().flat_map(|s| s.samples).collect();
        let flushed: Vec<f32> = seg.flush().into_iter().flat_map(|s| s.samples).collect();
        let total: Vec<f32> = produced.into_iter().chain(flushed).collect();

        // Overlap causes 12 extra samples (6 segments × 2 overlap) + 0 from flush
        assert_eq!(total.len(), 62, "overlap adds 12 duplicates");

        let covered_end = total.iter().cloned().fold(f32::NEG_INFINITY, f32::max) as usize;
        assert!(covered_end >= 49, "last input sample must appear in output");
    }

    #[test]
    fn test_segment_global_times_monotonic() {
        let mut seg = Segmenter::new(config_custom(1000, 100, 10));
        let samples: Vec<f32> = vec![0.0; 500];
        let segments = seg.feed(&samples);

        for i in 1..segments.len() {
            assert!(
                segments[i].global_start_ms > segments[i - 1].global_start_ms,
                "segment {} start_ms {} should be > segment {} start_ms {}",
                i,
                segments[i].global_start_ms,
                i - 1,
                segments[i - 1].global_start_ms,
            );
            assert_eq!(
                segments[i].global_end_ms - segments[i].global_start_ms,
                100,
                "segment {} duration should be 100ms",
                i,
            );
        }
    }
}
