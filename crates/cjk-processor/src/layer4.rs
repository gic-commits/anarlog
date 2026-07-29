use std::collections::HashMap;

use crate::types::{ProcessedResult, PunctuationLabel, WordEntry, WordGroup};

use super::layer2::JiebaGroup;

pub fn build_output(
    words: &[WordEntry],
    final_groups: &[JiebaGroup],
    punctuation: &[PunctuationLabel],
) -> ProcessedResult {
    let punct_map: HashMap<usize, &str> = punctuation
        .iter()
        .map(|p| (p.position, p.punct.as_str()))
        .collect();

    let mut word_groups: Vec<WordGroup> = Vec::with_capacity(final_groups.len());

    for &(start_idx, end_idx, ref text) in final_groups {
        let chars: Vec<WordEntry> = words[start_idx..end_idx].to_vec();
        if chars.is_empty() {
            continue;
        }
        let group_start = chars[0].start;
        let group_end = chars.last().unwrap().end;
        let mut group_text = text.clone();

        // Append punctuation if the last character has one
        if let Some(punct) = punct_map.get(&(end_idx - 1)) {
            group_text.push_str(punct);
        }

        word_groups.push(WordGroup {
            text: group_text,
            start: group_start,
            end: group_end,
            chars,
        });
    }

    let text_parts: Vec<&str> = word_groups.iter().map(|g| g.text.as_str()).collect();
    let final_text = text_parts.join(" ");

    let segment_boundaries: Vec<usize> = punctuation
        .iter()
        .filter(|p| p.punct == "。")
        .map(|p| p.position)
        .collect();

    ProcessedResult {
        text: final_text,
        word_groups,
        punctuation: punctuation.to_vec(),
        segment_boundaries,
    }
}
