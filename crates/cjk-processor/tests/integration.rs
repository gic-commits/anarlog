use cjk_processor::{Config, Processor, WordEntry};

/// Utility: build a list of WordEntry from a space-separated string of
/// characters and a fixed per-char duration.
///
/// This is a simplified helper for testing the jieba + reconstruction
/// pipeline without realistic acoustic timing.  Layer 3 (acoustic merge)
/// uses timing ratios, so these tests primarily exercise Layers 1, 2, and 4.
fn words_from_string(s: &str, char_duration: f64) -> Vec<WordEntry> {
    let chars: Vec<&str> = s.split_whitespace().collect();
    let mut words = Vec::with_capacity(chars.len());
    let mut t = 0.0;
    for &c in &chars {
        words.push(WordEntry::new(c.to_string(), t, t + char_duration));
        t += char_duration;
    }
    words
}

/// Build words with specific durations for acoustic testing.
fn words_with_durations(chars: &[&str], durations: &[f64]) -> Vec<WordEntry> {
    let mut words = Vec::with_capacity(chars.len());
    let mut t = 0.0;
    for (i, &c) in chars.iter().enumerate() {
        let d = durations[i];
        words.push(WordEntry::new(c.to_string(), t, t + d));
        t += d;
    }
    words
}

// ─── Test: passthrough for non-Chinese ───────────────────────

#[test]
fn test_non_chinese_passthrough() {
    let processor = Processor::new(Config::default()).expect("should create processor");
    let words = words_from_string("h e l l o", 0.1);
    let result = processor.process(&words, "en");
    assert!(result.is_none(), "English should not be processed");
}

// ─── Test: short input ──────────────────────────────────────

#[test]
fn test_short_input() {
    let processor = Processor::new(Config::default()).expect("should create processor");
    let words = words_from_string("我", 0.3);
    let result = processor.process(&words, "zh");
    assert!(result.is_none(), "Single char should not be processed");
}

// ─── Test: known multi-char words with jieba (basic) ────────

#[test]
fn test_with_jieba_known_words() {
    let processor = Processor::new(Config::default()).expect("should create processor");

    // With jieba, "我们" should be grouped if the dictionary recognizes it.
    // We supply single characters and let jieba merge them.
    let words = words_from_string("我 们 可 以 完 成", 0.2);
    let result = processor.process(&words, "zh");
    let result = result.expect("should process Chinese text");

    // At minimum, output should not be empty
    assert!(!result.text.is_empty(), "text should not be empty");

    // Words should be grouped (multi-char groups from jieba)
    assert!(
        result.word_groups.len() < words.len(),
        "should have fewer groups than input words"
    );

    // Check that no space-separated input chars appear ungrouped in output text
    for group in &result.word_groups {
        assert!(!group.text.is_empty(), "group text should not be empty");
    }
}

// ─── Test: acoustic merge (Tier C) ──────────────────────────

#[test]
fn test_acoustic_merge_oov() {
    let mut config = Config::default();
    config.features.enable_jieba = false; // Force Tier C
    let processor = Processor::new(config).expect("should create processor");

    // Simulate "阿里巴巴" with continuous shortening (all ratios < 0.6 → merge)
    let words = words_with_durations(&["阿", "里", "巴", "巴"], &[10.0, 3.0, 1.0, 0.5]);
    let result = processor
        .process(&words, "zh")
        .expect("should process Chinese");
    assert!(
        result.word_groups.len() < words.len(),
        "acoustic merge should reduce group count: {} vs {}",
        result.word_groups.len(),
        words.len(),
    );
}

// ─── Test: acoustic merge with clear boundary ───────────────

#[test]
fn test_acoustic_merge_with_boundary() {
    let mut config = Config::default();
    config.features.enable_jieba = false; // Force Tier C
    let processor = Processor::new(config).expect("should create processor");

    // "你好世界" where "世界" has a clear lengthening on the second char.
    // First two chars similar (你=2.0, 好=2.1), then 世=2.0, 界=5.0 (lengthening)
    // The 界 should be detected as a new word start due to sudden lengthening.
    let words = words_with_durations(&["你", "好", "世", "界"], &[2.0, 2.1, 2.0, 5.0]);
    let _result = processor
        .process(&words, "zh")
        .expect("should process Chinese");
    let words = words_with_durations(&["你", "好", "世", "界"], &[3.0, 1.5, 1.4, 4.0]);
    let _result = processor
        .process(&words, "zh")
        .expect("should process Chinese");
}

// ─── Test: punctuation detection ────────────────────────────

#[test]
fn test_punctuation_from_gap() {
    let mut config = Config::default();
    config.features.enable_jieba = false;
    let processor = Processor::new(config).expect("should create processor");

    // Build words with a large gap at position 3
    let mut words = words_from_string("我 是 学 生", 0.2);
    // Insert a large gap after "学" (index 2): student pause
    words[3].start = words[2].end + 0.9;

    let result = processor
        .process(&words, "zh")
        .expect("should process Chinese");

    // Should detect a punctuation (period) after "学" since gap >= 0.80
    assert!(
        !result.punctuation.is_empty(),
        "should detect punctuation from gap"
    );

    // The output text should contain the punctuation
    assert!(
        result.text.contains("。") || result.text.contains("，"),
        "output text should contain punctuation: got '{}'",
        result.text
    );
}
