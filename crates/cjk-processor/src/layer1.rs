use crate::config::Config;
use crate::types::{PunctuationLabel, WordEntry};

pub fn detect_punctuation(words: &[WordEntry], config: &Config) -> Vec<PunctuationLabel> {
    let mut result = Vec::new();
    for i in 1..words.len() {
        let gap = words[i].start - words[i - 1].end;
        if gap >= config.sentence_gap {
            result.push(PunctuationLabel {
                position: i,
                punct: "。".to_string(),
            });
        } else if gap >= config.clause_gap {
            result.push(PunctuationLabel {
                position: i,
                punct: "，".to_string(),
            });
        }
    }
    result
}
