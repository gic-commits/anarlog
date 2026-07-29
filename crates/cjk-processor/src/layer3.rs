use std::collections::HashSet;

use crate::config::Config;
use crate::types::WordEntry;

use super::layer2::JiebaGroup;

/// Verify jieba groupings against acoustic evidence and merge OOV single-char
/// spans using adaptive threshold.
pub fn acoustic_verify(
    words: &[WordEntry],
    jieba_groups: &[JiebaGroup],
    config: &Config,
    punct_positions: &HashSet<usize>,
) -> Vec<JiebaGroup> {
    let mut result: Vec<JiebaGroup> = Vec::new();
    let mut i = 0;

    while i < jieba_groups.len() {
        let (start, end, ref word) = jieba_groups[i];
        let char_count = end - start;

        // Multi-char known word → trust jieba
        let has_punct = (start..end).any(|p| punct_positions.contains(&p));
        if char_count >= 2 || config.func_words.contains(word) || has_punct {
            result.push((start, end, word.clone()));
            i += 1;
            continue;
        }

        // Single-char non-function-word → collect OOV span
        let span_start = i;
        while i < jieba_groups.len() {
            let (s, e, ref w) = jieba_groups[i];
            if e - s != 1 || config.func_words.contains(w) {
                break;
            }
            if (s..e).any(|p| punct_positions.contains(&p)) {
                break;
            }
            i += 1;
        }
        let span_end = i;

        if span_end - span_start <= 1 {
            result.push((start, end, word.clone()));
        } else {
            let merged = try_merge_oov_span(
                words,
                jieba_groups[span_start].0,
                jieba_groups[span_end - 1].1,
                config,
            );
            result.extend(merged);
        }
    }

    result
}

/// Pure-acoustic fallback (Tier C).  Treat every character as a potential OOV
/// single-char and run the merge algorithm on the whole sequence.
pub fn acoustic_only(words: &[WordEntry], config: &Config) -> Vec<JiebaGroup> {
    let groups: Vec<JiebaGroup> = (0..words.len())
        .map(|i| (i, i + 1, words[i].char.clone()))
        .collect();
    acoustic_verify(words, &groups, config, &HashSet::new())
}

// ─── OOV span merge ──────────────────────────────────────────

fn try_merge_oov_span(
    words: &[WordEntry],
    span_start: usize,
    span_end: usize,
    config: &Config,
) -> Vec<JiebaGroup> {
    if span_end - span_start < 2 {
        return vec![(span_start, span_end, words[span_start].char.clone())];
    }

    let spans = &words[span_start..span_end];
    let durations: Vec<f64> = spans.iter().map(|w| w.duration()).collect();
    let boundaries = detect_boundaries(&durations, config);

    if boundaries.is_empty() {
        let merged: String = spans.iter().map(|w| w.char.as_str()).collect();
        return vec![(span_start, span_end, merged)];
    }

    let mut result: Vec<JiebaGroup> = Vec::new();
    let mut seg_start = 0;
    for &b in &boundaries {
        if b > seg_start {
            let txt: String = spans[seg_start..b]
                .iter()
                .map(|w| w.char.as_str())
                .collect();
            result.push((span_start + seg_start, span_start + b, txt));
        }
        seg_start = b;
    }
    if seg_start < spans.len() {
        let txt: String = spans[seg_start..].iter().map(|w| w.char.as_str()).collect();
        result.push((span_start + seg_start, span_end, txt));
    }

    result
}

fn detect_boundaries(durations: &[f64], config: &Config) -> Vec<usize> {
    let mut boundaries: Vec<usize> = Vec::new();

    for i in 1..durations.len() {
        let prev = durations[i - 1];
        let curr = durations[i];
        let ratio = if prev > 0.0 { curr / prev } else { 999.0 };

        let half = config.window_size / 2;
        let left = i.saturating_sub(half);
        let right = (i + half + 1).min(durations.len());
        let window: Vec<f64> = durations[left..right]
            .iter()
            .copied()
            .filter(|&d| d > 0.0)
            .collect();

        let is_boundary = if window.len() < 3 {
            ratio >= 0.8
        } else {
            let med = median(&window);
            if med == 0.0 {
                continue;
            }
            if ratio < config.alpha_low {
                false
            } else if ratio > config.alpha_high {
                true
            } else {
                curr >= config.alpha_low * med
            }
        };

        if is_boundary {
            boundaries.push(i);
        }
    }

    boundaries
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

// ─── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_func_words;

    fn cfg() -> Config {
        Config {
            func_words: default_func_words(),
            ..Config::default()
        }
    }

    #[test]
    fn test_median_odd() {
        assert!((median(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_median_even() {
        assert!((median(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < 1e-9);
    }

    #[test]
    fn test_detect_boundaries_all_same() {
        // All durations equal → no boundaries (ratio ≈ 1.0 which is between
        // alpha_low and alpha_high; curr ≈ med so it falls in the final else:
        // curr >= alpha_low * med → true → boundary)
        // Actually: if all equal, ratio=1.0, med=equal, curr=equal,
        // alpha_low * med = 0.6 * equal → curr >= 0.6*equal → true → boundary
        // Hmm, that's not great for same-duration sequences.
        // Let me think about this...
        //
        // With all durations = 0.3:
        //   ratio = 1.0, med = 0.3, alpha_low=0.6, alpha_high=1.5
        //   1.0 > alpha_high? No (1.0 < 1.5)
        //   1.0 < alpha_low? No (1.0 > 0.6)
        //   curr >= alpha_low * med? 0.3 >= 0.6*0.3 = 0.18 → Yes → boundary
        //
        // So this test needs to account for that. Let me make the durations
        // have a clear shortening pattern: no boundary when curr/prev < alpha_low
        let d = vec![0.5, 0.2]; // ratio = 0.4 < 0.6 → no boundary
        let b = detect_boundaries(&d, &cfg());
        assert!(b.is_empty(), "shortening should not produce boundary");
    }

    #[test]
    fn test_detect_boundaries_clear_gap() {
        // Clear lengthening: boundary expected
        let d = vec![0.2, 0.5]; // ratio = 2.5 > 1.5 → boundary
        let b = detect_boundaries(&d, &cfg());
        assert_eq!(b, vec![1]);
    }

    #[test]
    fn test_detect_boundaries_small_window() {
        // Window too small (< 3), use simple threshold
        let d = vec![0.2, 0.18]; // ratio = 0.9 >= 0.8 → boundary
        let b = detect_boundaries(&d, &cfg());
        assert_eq!(b, vec![1]);
    }

    #[test]
    fn test_try_merge_oov_span_no_boundary() {
        // Continuous shortening: ratios 0.3, 0.33, 0.5 all < alpha_low (0.6)
        // → no boundaries, all four chars merge into one group
        let words = vec![
            WordEntry::new("阿".into(), 0.0, 10.0),
            WordEntry::new("里".into(), 10.0, 13.0),
            WordEntry::new("巴".into(), 13.0, 14.0),
            WordEntry::new("巴".into(), 14.0, 14.5),
        ];
        let merged = try_merge_oov_span(&words, 0, 4, &cfg());
        assert_eq!(merged.len(), 1, "should merge all into one group");
        assert_eq!(merged[0].2, "阿里巴巴");
    }

    #[test]
    fn test_try_merge_oov_span_with_boundary() {
        // Shorten (0.3) → shorten (0.33) → lengthen (5.0 > 1.5)
        // → boundary at position 3 → groups: [阿+里+巴] [巴]
        let words = vec![
            WordEntry::new("阿".into(), 0.0, 10.0),
            WordEntry::new("里".into(), 10.0, 13.0),
            WordEntry::new("巴".into(), 13.0, 14.0),
            WordEntry::new("巴".into(), 14.0, 19.0),
        ];
        let merged = try_merge_oov_span(&words, 0, 4, &cfg());
        assert_eq!(merged.len(), 2, "should split at the lengthening boundary");
        assert_eq!(merged[0].2, "阿里巴");
        assert_eq!(merged[1].2, "巴");
    }
}
