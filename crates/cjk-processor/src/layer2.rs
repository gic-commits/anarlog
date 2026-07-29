use crate::types::WordEntry;

pub type JiebaGroup = (usize, usize, String);

/// Run jieba segmentation on the concatenated text of all words.
///
/// Returns a list of `(char_start, char_end, word_text)` tuples where
/// indices refer to positions in the `words` slice (character-level,
/// already returned as such by jieba-rs `tokenize`).
pub fn jieba_segment(words: &[WordEntry], jieba: &jieba_rs::Jieba) -> Vec<JiebaGroup> {
    let text: String = words.iter().map(|w| w.char.as_str()).collect();

    let tokens = jieba.tokenize(&text, jieba_rs::TokenizeMode::Default, true);

    let mut groups: Vec<JiebaGroup> = Vec::with_capacity(tokens.len());
    for tok in tokens {
        // start/end are character-level indices, not byte offsets
        groups.push((tok.start, tok.end, tok.word.to_string()));
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jieba() -> jieba_rs::Jieba {
        jieba_rs::Jieba::new()
    }

    #[test]
    fn test_jieba_segment_simple() {
        let words = vec![
            WordEntry::new("我".into(), 0.0, 0.3),
            WordEntry::new("们".into(), 0.3, 0.6),
            WordEntry::new("可".into(), 0.6, 0.9),
            WordEntry::new("以".into(), 0.9, 1.2),
            WordEntry::new("完".into(), 1.2, 1.5),
            WordEntry::new("成".into(), 1.5, 1.8),
        ];
        let groups = jieba_segment(&words, &jieba());

        // jieba should group "我们", "可以", "完成"
        // Each group corresponds to 2 chars at consecutive indices
        let all_merged: Vec<usize> = groups.iter().map(|(s, e, _)| e - s).collect();
        assert!(
            all_merged.iter().any(|&len| len >= 2),
            "jieba should produce some multi-char groups: {:?}",
            groups
        );

        // Total chars should match
        let total_chars: usize = groups.iter().map(|(s, e, _)| e - s).sum();
        assert_eq!(total_chars, words.len());
    }
}
