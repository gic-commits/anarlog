use std::collections::HashMap;
use std::sync::Mutex;

use hypr_cjk_processor::{CjkFeatures, Config, ProcessedResult, Processor, WordEntry};
use owhisper_interface::batch;

use super::CjkLayerFlags;

impl From<CjkLayerFlags> for CjkFeatures {
    fn from(f: CjkLayerFlags) -> Self {
        Self {
            enable_punctuation: f.punctuation,
            enable_jieba: f.jieba,
            enable_acoustic_merge: f.acoustic_merge,
        }
    }
}

fn with_processor(features: CjkFeatures, f: impl FnOnce(&Processor)) {
    use std::sync::LazyLock;
    static CACHE: LazyLock<Mutex<(Processor, CjkFeatures)>> = std::sync::LazyLock::new(|| {
        let mut cfg = Config::default();
        cfg.features = CjkFeatures::default();
        Mutex::new((
            Processor::new(cfg).expect("failed to create CJK processor"),
            CjkFeatures::default(),
        ))
    });
    let mut guard = CACHE.lock().expect("cjk processor cache poisoned");
    if guard.1 != features {
        let mut cfg = Config::default();
        cfg.features = features;
        *guard = (
            Processor::new(cfg).expect("failed to create CJK processor"),
            features,
        );
    }
    f(&guard.0);
}

fn is_cjk_punct(c: char) -> bool {
    matches!(
        c,
        '。' | '，' | '、' | '；' | '：' | '？' | '！' | '—' | '…' | '～'
    )
}

fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
    )
}

fn split_to_entries(
    words: &[batch::Word],
    min_cjk_split_len: usize,
) -> (
    Vec<WordEntry>,
    HashMap<(i64, i64), usize>,
    Vec<(usize, usize)>,
) {
    let mut entries = Vec::new();
    let mut char_to_word: HashMap<(i64, i64), usize> = HashMap::new();
    let mut atomic_ranges: Vec<(usize, usize)> = Vec::new();

    for (word_idx, word) in words.iter().enumerate() {
        let chars: Vec<char> = word.word.chars().collect();

        if chars.len() <= 1 || !chars.iter().any(|&c| is_cjk_char(c)) {
            let key = ((word.start * 1000.0) as i64, (word.end * 1000.0) as i64);
            entries.push(WordEntry::new(word.word.clone(), word.start, word.end));
            char_to_word.insert(key, word_idx);
        } else {
            let is_atomic = chars.len() < min_cjk_split_len;
            let entry_start = entries.len();
            let duration = (word.end - word.start) / chars.len() as f64;
            let mut t = word.start;

            for &c in &chars {
                let key = ((t * 1000.0) as i64, ((t + duration) * 1000.0) as i64);
                entries.push(WordEntry::new(c.to_string(), t, t + duration));
                char_to_word.insert(key, word_idx);
                t += duration;
            }

            if is_atomic {
                atomic_ranges.push((entry_start, entries.len()));
            }
        }
    }

    (entries, char_to_word, atomic_ranges)
}

fn reconstruct_from_groups(
    groups: &[hypr_cjk_processor::WordGroup],
    char_to_word: &HashMap<(i64, i64), usize>,
    original_words: &[batch::Word],
) -> Vec<batch::Word> {
    groups
        .iter()
        .map(|group| {
            let text = &group.text;

            let stripped = text.trim_end_matches(is_cjk_punct);
            let (word, punctuated_word) = if stripped.len() < text.len() {
                (stripped.to_string(), Some(text.clone()))
            } else {
                (text.clone(), None)
            };

            let mut confidence = 1.0f64;
            let mut channel = 0i32;
            let mut speaker = None;

            if let Some(first_char) = group.chars.first() {
                let key = (
                    (first_char.start * 1000.0) as i64,
                    (first_char.end * 1000.0) as i64,
                );
                if let Some(&orig_idx) = char_to_word.get(&key) {
                    if orig_idx < original_words.len() {
                        let orig = &original_words[orig_idx];
                        confidence = orig.confidence;
                        channel = orig.channel;
                        speaker = orig.speaker;
                    }
                }
            }

            batch::Word {
                word,
                start: group.start,
                end: group.end,
                confidence,
                channel,
                speaker,
                punctuated_word,
            }
        })
        .collect()
}

fn update_transcript_from_groups(groups: &[hypr_cjk_processor::WordGroup]) -> String {
    groups
        .iter()
        .map(|g| g.text.as_str())
        .collect::<Vec<&str>>()
        .join(" ")
}

fn collapse_groups(
    groups: Vec<hypr_cjk_processor::WordGroup>,
    entries: &[WordEntry],
    atomic_ranges: &[(usize, usize)],
    original_words: &[batch::Word],
    char_to_word: &HashMap<(i64, i64), usize>,
) -> Vec<hypr_cjk_processor::WordGroup> {
    if atomic_ranges.is_empty() {
        return groups;
    }

    let range_timing: Vec<(usize, usize, f64, f64)> = atomic_ranges
        .iter()
        .map(|&(s, e)| (s, e, entries[s].start, entries[e - 1].end))
        .collect();

    let mut out = Vec::with_capacity(groups.len());
    let mut i = 0;
    while i < groups.len() {
        let g = &groups[i];

        let range = range_timing
            .iter()
            .find(|(_, _, r_s, r_e)| *r_s <= g.start && g.end <= *r_e);

        if let Some(&(r_s, r_e, _, _)) = range {
            let mut merged_chars: Vec<WordEntry> = Vec::new();
            let merged_start = g.start;
            let mut merged_end = g.end;

            let mut j = i;
            while j < groups.len() {
                let gj = &groups[j];
                if gj.start >= entries[r_s].start - 1e-9 && gj.end <= entries[r_e - 1].end + 1e-9 {
                    merged_chars.extend(gj.chars.clone());
                    merged_end = gj.end;
                    j += 1;
                } else {
                    break;
                }
            }

            let orig_key = (
                (entries[r_s].start * 1000.0) as i64,
                (entries[r_s].end * 1000.0) as i64,
            );
            let orig_text = char_to_word
                .get(&orig_key)
                .and_then(|&idx| original_words.get(idx))
                .map(|w| w.word.as_str())
                .unwrap_or("");

            let trailing_punct: String = groups[j - 1]
                .text
                .chars()
                .skip_while(|c| !is_cjk_punct(*c))
                .collect();

            let final_text = if trailing_punct.is_empty() {
                orig_text.to_string()
            } else {
                format!("{}{}", orig_text, trailing_punct)
            };

            out.push(hypr_cjk_processor::WordGroup {
                text: final_text,
                start: merged_start,
                end: merged_end,
                chars: merged_chars,
            });

            i = j;
        } else {
            out.push(g.clone());
            i += 1;
        }
    }

    out
}

pub fn process_response(
    response: &batch::Response,
    language: &str,
    features: Option<CjkLayerFlags>,
) -> Option<batch::Response> {
    if !language.starts_with("zh") {
        return None;
    }

    let alt = &response.results.channels[0].alternatives[0];
    if alt.words.len() < 2 {
        return None;
    }

    let flags = features.unwrap_or_default();
    let min_cjk_split_len = if flags.jieba { 5 } else { usize::MAX };
    let (entries, char_to_word, atomic_ranges) = split_to_entries(&alt.words, min_cjk_split_len);

    let cjk_features: CjkFeatures = flags.into();
    let mut result: Option<ProcessedResult> = None;
    with_processor(cjk_features, |processor| {
        result = processor.process(&entries, language);
    });
    let result: ProcessedResult = result?;

    let word_groups = collapse_groups(
        result.word_groups,
        &entries,
        &atomic_ranges,
        &alt.words,
        &char_to_word,
    );

    tracing::info!(
        "[cjk] processed: {} words -> {} groups ({} atomic ranges collapsed), {} punct, {} sent boundaries",
        alt.words.len(),
        word_groups.len(),
        atomic_ranges.len(),
        result.punctuation.len(),
        result.segment_boundaries.len(),
    );

    let new_words = reconstruct_from_groups(&word_groups, &char_to_word, &alt.words);
    let new_transcript = update_transcript_from_groups(&word_groups);

    let mut new_response = response.clone();
    new_response.results.channels[0].alternatives[0].words = new_words;
    new_response.results.channels[0].alternatives[0].transcript = new_transcript;

    if let Some(obj) = new_response.metadata.as_object_mut() {
        obj.insert("cjk_processed".to_string(), serde_json::json!(true));
        if !result.segment_boundaries.is_empty() {
            obj.insert(
                "cjk_segment_boundaries".to_string(),
                serde_json::json!(result.segment_boundaries),
            );
        }
    }

    Some(new_response)
}
