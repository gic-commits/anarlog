use std::collections::BTreeMap;

use owhisper_interface::batch::{self, Response};

const DEDUP_EPSILON_S: f64 = 0.05;
const GAP_WARNING_THRESHOLD_MS: i64 = 2000;

#[derive(Debug, Clone)]
pub struct StitcherConfig {
    pub overlap_ms: u64,
    pub segment_duration_ms: u64,
    pub total_segments: usize,
}

impl Default for StitcherConfig {
    fn default() -> Self {
        Self {
            overlap_ms: 1000,
            segment_duration_ms: 30000,
            total_segments: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletedSegment {
    pub index: usize,
    pub global_start_ms: i64,
    pub response: Response,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StitcherError {
    EmptyResponse,
}

#[derive(Debug)]
pub struct Stitcher {
    config: StitcherConfig,
    segments: BTreeMap<usize, CompletedSegment>,
    abandoned: Vec<usize>,
}

impl Stitcher {
    pub fn new(config: StitcherConfig) -> Self {
        Self {
            config,
            segments: BTreeMap::new(),
            abandoned: Vec::new(),
        }
    }

    pub fn add_segment(&mut self, segment: CompletedSegment) {
        self.segments.insert(segment.index, segment);
    }

    pub fn add_abandoned(&mut self, index: usize) {
        if self.segments.contains_key(&index) {
            return;
        }
        if !self.abandoned.contains(&index) {
            self.abandoned.push(index);
        }
    }

    pub fn contains(&self, index: usize) -> bool {
        self.segments.contains_key(&index)
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    pub fn abandoned_indices(&self) -> &[usize] {
        &self.abandoned
    }

    pub fn is_complete(&self) -> bool {
        if self.segments.is_empty() {
            return false;
        }
        if self.config.total_segments > 0 {
            self.segments.len() == self.config.total_segments
        } else {
            let max_index = self.segments.keys().max().copied().unwrap_or(0);
            self.segments.len() == max_index + 1
        }
    }

    pub fn stitch(&self) -> Result<Response, StitcherError> {
        if self.segments.is_empty() {
            return Err(StitcherError::EmptyResponse);
        }

        let max_index = self.segments.keys().max().copied().unwrap_or(0);
        let segments_total = if self.config.total_segments > 0 {
            self.config.total_segments
        } else {
            max_index + 1
        };
        let missing: Vec<usize> = if self.abandoned.is_empty() {
            (0..segments_total)
                .filter(|i| !self.segments.contains_key(i))
                .collect()
        } else {
            self.abandoned
                .iter()
                .filter(|i| !self.segments.contains_key(i))
                .copied()
                .collect()
        };

        struct TaggedWord {
            word: batch::Word,
            seg_index: usize,
        }

        let mut tagged: Vec<TaggedWord> = Vec::new();
        let mut gaps: Vec<(usize, i64)> = Vec::new();
        let mut prev_expected_end_ms: Option<i64> = None;

        for (index, segment) in &self.segments {
            let alt = match segment.response.results.channels.first() {
                Some(ch) => ch.alternatives.first(),
                None => return Err(StitcherError::EmptyResponse),
            };

            let words = match alt {
                Some(a) => &a.words,
                None => return Err(StitcherError::EmptyResponse),
            };

            if words.is_empty() {
                continue;
            }

            // Gap detection: if this segment starts significantly later than
            // the previous segment's expected end, record a gap warning.
            if let Some(prev_end) = prev_expected_end_ms {
                let gap_ms = segment.global_start_ms - prev_end;
                if gap_ms > GAP_WARNING_THRESHOLD_MS {
                    gaps.push((*index, gap_ms));
                }
            }
            prev_expected_end_ms =
                Some(segment.global_start_ms + self.config.segment_duration_ms as i64);

            let offset_secs = segment.global_start_ms as f64 / 1000.0;

            let prev_max_end = tagged
                .last()
                .map(|w: &TaggedWord| w.word.end)
                .unwrap_or(f64::NEG_INFINITY);

            let overlap_secs = self.config.overlap_ms as f64 / 1000.0;
            let overlap_zone_start = prev_max_end - overlap_secs;

            for word in words {
                let mut w = word.clone();
                w.start += offset_secs;
                w.end += offset_secs;

                if *index > 0
                    && w.start >= overlap_zone_start
                    && w.end <= prev_max_end + DEDUP_EPSILON_S
                {
                    continue;
                }

                tagged.push(TaggedWord {
                    word: w,
                    seg_index: *index,
                });
            }
        }

        if tagged.is_empty() {
            return Err(StitcherError::EmptyResponse);
        }

        tagged.sort_by(|a, b| a.word.start.partial_cmp(&b.word.start).unwrap());

        // Record word indices where a new segment contributes its first word.
        let mut boundaries: Vec<usize> = Vec::new();
        boundaries.push(0);
        for i in 1..tagged.len() {
            if tagged[i].seg_index != tagged[i - 1].seg_index {
                boundaries.push(i);
            }
        }

        let all_words: Vec<batch::Word> = tagged.into_iter().map(|t| t.word).collect();

        let transcript = all_words
            .iter()
            .map(|w| w.punctuated_word.as_deref().unwrap_or(&w.word))
            .collect::<Vec<_>>()
            .join(" ");

        let total_duration = all_words.last().map(|w| w.end).unwrap_or(0.0);

        let mut metadata = serde_json::json!({
            "total_duration": total_duration,
            "segments_stitched": self.segments.len(),
            "segments_total": segments_total,
            "segment_boundaries": boundaries,
        });

        if !missing.is_empty() {
            metadata["abandoned_segments"] = serde_json::json!(missing);
        }

        if !gaps.is_empty() {
            let gap_warnings: Vec<serde_json::Value> = gaps
                .iter()
                .map(|(idx, ms)| {
                    serde_json::json!({
                        "after_segment": idx - 1,
                        "before_segment": idx,
                        "gap_ms": ms,
                    })
                })
                .collect();
            metadata["gap_warnings"] = serde_json::json!(gap_warnings);
        }

        Ok(Response {
            metadata,
            results: batch::Results {
                channels: vec![batch::Channel {
                    alternatives: vec![batch::Alternatives {
                        transcript,
                        confidence: 0.0,
                        words: all_words,
                    }],
                }],
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(overlap_ms: u64) -> StitcherConfig {
        StitcherConfig {
            overlap_ms,
            segment_duration_ms: 30000,
            total_segments: 0,
        }
    }

    fn make_word(start: f64, end: f64, text: &str) -> batch::Word {
        batch::Word {
            word: text.to_string(),
            start,
            end,
            confidence: 0.95,
            channel: 0,
            speaker: None,
            punctuated_word: Some(text.to_string()),
        }
    }

    fn make_response(words: Vec<batch::Word>) -> Response {
        Response {
            metadata: serde_json::json!({}),
            results: batch::Results {
                channels: vec![batch::Channel {
                    alternatives: vec![batch::Alternatives {
                        transcript: words
                            .iter()
                            .map(|w| w.word.as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                        confidence: 0.95,
                        words,
                    }],
                }],
            },
        }
    }

    fn seg(index: usize, start_ms: i64, words: Vec<batch::Word>) -> CompletedSegment {
        CompletedSegment {
            index,
            global_start_ms: start_ms,
            response: make_response(words),
        }
    }

    #[test]
    fn test_empty_on_no_segments() {
        let stitcher = Stitcher::new(config(1000));
        assert_eq!(stitcher.stitch(), Err(StitcherError::EmptyResponse));
    }

    #[test]
    fn test_not_complete_with_missing_segments() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "hello")]));
        stitcher.add_segment(seg(2, 58000, vec![make_word(0.0, 1.0, "world")]));
        assert!(!stitcher.is_complete());
        let result = stitcher.stitch().unwrap();
        let abandoned = result.metadata["abandoned_segments"].as_array().unwrap();
        assert!(abandoned.contains(&serde_json::json!(1)));
    }

    #[test]
    fn test_is_complete_when_all_segments_present() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "hello")]));
        stitcher.add_segment(seg(1, 29000, vec![make_word(0.0, 1.0, "world")]));
        assert!(stitcher.is_complete());
    }

    #[test]
    fn test_global_offset_uses_global_start_ms() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 0.5, "hello")]));
        // Segment 1 starts at 29500ms (non-standard offset, simulating VAD boundary)
        stitcher.add_segment(seg(1, 29500, vec![make_word(0.0, 0.5, "world")]));
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert_eq!(words.len(), 2);
        assert!((words[0].start - 0.0).abs() < 0.001);
        assert!((words[1].start - 29.5).abs() < 0.001);
        assert!((words[1].end - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_overlap_deduplication_uses_global_start_ms() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(
            0,
            0,
            vec![make_word(0.0, 1.0, "hello"), make_word(29.0, 30.0, "world")],
        ));
        // Segment 1 with non-uniform start — dedup zone shifts accordingly
        stitcher.add_segment(seg(
            1,
            29000,
            vec![make_word(0.0, 0.8, "world"), make_word(0.8, 1.6, "today")],
        ));
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].word, "hello");
        assert_eq!(words[1].word, "world");
        assert_eq!(words[2].word, "today");
        assert!((words[1].start - 29.0).abs() < 0.001);
        assert!((words[2].start - 29.8).abs() < 0.001);
    }

    #[test]
    fn test_non_uniform_segment_starts_preserve_content() {
        // Simulate VAD boundary misalignment: segment 1 starts at 31000ms
        // instead of the ideal 29000ms. The stitcher should correctly
        // offset words and not leave gaps or drop content.
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(
            0,
            0,
            vec![make_word(0.0, 1.0, "hello"), make_word(28.0, 29.0, "world")],
        ));
        stitcher.add_segment(seg(
            1,
            31000,
            vec![make_word(0.0, 1.0, "today"), make_word(1.0, 2.0, "is")],
        ));
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert_eq!(words.len(), 4);
        assert_eq!(words[0].word, "hello");
        assert!((words[0].start - 0.0).abs() < 0.001);
        assert_eq!(words[1].word, "world");
        assert!((words[1].start - 28.0).abs() < 0.001);
        assert_eq!(words[2].word, "today");
        assert!((words[2].start - 31.0).abs() < 0.001);
        assert_eq!(words[3].word, "is");
        assert!((words[3].start - 32.0).abs() < 0.001);
    }

    #[test]
    fn test_gap_detection_records_warning() {
        // Two segments with a 5s gap (well beyond 2s threshold)
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "hello")]));
        stitcher.add_segment(seg(1, 64000, vec![make_word(0.0, 1.0, "later")]));
        let result = stitcher.stitch().unwrap();
        let gaps = result.metadata["gap_warnings"].as_array();
        assert!(gaps.is_some(), "should have gap warnings");
        assert_eq!(gaps.unwrap().len(), 1);
        assert_eq!(gaps.unwrap()[0]["gap_ms"].as_i64(), Some(34000));
    }

    #[test]
    fn test_no_gap_warning_when_segments_contiguous() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "hello")]));
        stitcher.add_segment(seg(1, 29000, vec![make_word(0.0, 1.0, "world")]));
        let result = stitcher.stitch().unwrap();
        assert!(result.metadata.get("gap_warnings").is_none());
    }

    #[test]
    fn test_non_overlapping_segments_all_kept() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 25.0, "hello")]));
        stitcher.add_segment(seg(1, 29000, vec![make_word(5.0, 10.0, "world")]));
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].word, "hello");
        assert_eq!(words[1].word, "world");
    }

    #[test]
    fn test_transcript_concatenation() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "Hello")]));
        stitcher.add_segment(seg(1, 29000, vec![make_word(0.0, 1.0, "world")]));
        let result = stitcher.stitch().unwrap();
        assert_eq!(
            result.results.channels[0].alternatives[0].transcript,
            "Hello world"
        );
    }

    #[test]
    fn test_words_sorted_by_global_start() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(1, 29000, vec![make_word(0.0, 1.0, "late")]));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "early")]));
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert_eq!(words[0].word, "early");
        assert_eq!(words[1].word, "late");
    }

    #[test]
    fn test_stitcher_empty_response_when_only_empty_words() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![]));
        assert_eq!(stitcher.stitch(), Err(StitcherError::EmptyResponse));
    }

    #[test]
    fn test_stitcher_empty_at_default_config() {
        let stitcher = Stitcher::new(StitcherConfig {
            overlap_ms: 1000,
            segment_duration_ms: 30000,
            total_segments: 0,
        });
        assert!(!stitcher.is_complete());
    }

    #[test]
    fn test_is_not_complete_with_gap() {
        let mut stitcher = Stitcher::new(StitcherConfig {
            overlap_ms: 1000,
            segment_duration_ms: 30000,
            total_segments: 0,
        });
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "hi")]));
        stitcher.add_segment(seg(2, 58000, vec![make_word(0.0, 1.0, "later")]));
        assert!(!stitcher.is_complete());
    }

    #[test]
    fn test_overlap_tolerance_keeps_words_just_beyond_threshold() {
        let mut stitcher = Stitcher::new(config(500));
        stitcher.add_segment(seg(
            0,
            0,
            vec![make_word(0.0, 10.0, "alpha"), make_word(10.0, 29.7, "beta")],
        ));
        stitcher.add_segment(seg(
            1,
            29000,
            vec![make_word(0.0, 0.3, "beta"), make_word(0.5, 1.0, "gamma")],
        ));
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert!(
            words.iter().any(|w| w.word == "gamma"),
            "gamma should be kept"
        );
    }

    #[test]
    fn test_words_from_non_uniform_start_do_not_dedup_across_boundary() {
        // Segment 1 starts at 34000ms — far from segment 0's end at 1000ms.
        // No overlap exists, so all words should be kept regardless of content.
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "hello")]));
        stitcher.add_segment(seg(1, 34000, vec![make_word(0.0, 1.0, "hello")]));
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].word, "hello");
        assert_eq!(words[0].start, 0.0);
        assert_eq!(words[1].word, "hello");
        assert!((words[1].start - 34.0).abs() < 0.001);
    }

    #[test]
    fn test_metadata_includes_segments_stitched_count() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "a")]));
        stitcher.add_segment(seg(1, 29000, vec![make_word(0.0, 1.0, "b")]));
        stitcher.add_segment(seg(2, 58000, vec![make_word(0.0, 1.0, "c")]));
        let result = stitcher.stitch().unwrap();
        assert_eq!(result.metadata["segments_stitched"].as_u64(), Some(3));
    }

    #[test]
    fn test_add_abandoned_does_not_override_completed() {
        let mut stitcher = Stitcher::new(config(1000));
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "a")]));
        // Try to abandon segment 0 after it was already completed
        stitcher.add_abandoned(0);
        let result = stitcher.stitch().unwrap();
        let words = &result.results.channels[0].alternatives[0].words;
        assert_eq!(words.len(), 1, "word should still be present");
        assert!(
            result.metadata.get("abandoned_segments").is_none(),
            "abandoned_segments should be empty"
        );
        assert!(stitcher.is_complete());
    }

    #[test]
    fn test_is_complete_with_total_segments() {
        let mut stitcher = Stitcher::new(StitcherConfig {
            overlap_ms: 1000,
            segment_duration_ms: 30000,
            total_segments: 5,
        });
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "a")]));
        stitcher.add_segment(seg(1, 29000, vec![make_word(0.0, 1.0, "b")]));
        assert!(!stitcher.is_complete(), "2/5 should not be complete");
        stitcher.add_segment(seg(2, 58000, vec![make_word(0.0, 1.0, "c")]));
        stitcher.add_segment(seg(3, 87000, vec![make_word(0.0, 1.0, "d")]));
        stitcher.add_segment(seg(4, 116000, vec![make_word(0.0, 1.0, "e")]));
        assert!(stitcher.is_complete(), "5/5 should be complete");
    }

    #[test]
    fn test_total_segments_in_metadata() {
        let mut stitcher = Stitcher::new(StitcherConfig {
            overlap_ms: 1000,
            segment_duration_ms: 30000,
            total_segments: 2,
        });
        stitcher.add_segment(seg(1, 29000, vec![make_word(0.0, 1.0, "b")]));
        // With total_segments=2, stitch should know segments 0 and 1
        // Segment 0 is missing → should be in abandoned_segments
        let result = stitcher.stitch().unwrap();
        assert_eq!(result.metadata["segments_total"].as_u64(), Some(2));
        let abandoned = result.metadata["abandoned_segments"].as_array().unwrap();
        assert!(
            abandoned.contains(&serde_json::json!(0)),
            "segment 0 should be abandoned"
        );
    }

    #[test]
    fn test_not_complete_with_gap_and_total_segments() {
        let mut stitcher = Stitcher::new(StitcherConfig {
            overlap_ms: 1000,
            segment_duration_ms: 30000,
            total_segments: 5,
        });
        stitcher.add_segment(seg(0, 0, vec![make_word(0.0, 1.0, "hi")]));
        stitcher.add_segment(seg(2, 58000, vec![make_word(0.0, 1.0, "later")]));
        assert!(!stitcher.is_complete(), "2/5 should not be complete");
        let result = stitcher.stitch().unwrap();
        let abandoned = result.metadata["abandoned_segments"].as_array().unwrap();
        // With total_segments=5, max_index=2 → missing = [1, 3, 4]
        assert!(abandoned.contains(&serde_json::json!(1)));
        assert!(abandoned.contains(&serde_json::json!(3)));
        assert!(abandoned.contains(&serde_json::json!(4)));
    }
}
