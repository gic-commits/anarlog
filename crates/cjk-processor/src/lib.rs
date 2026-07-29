mod config;
mod dict;
mod layer1;
mod layer2;
mod layer3;
mod layer4;
mod types;

pub use config::{CjkFeatures, Config};
pub use dict::{DictError, load_dicts};
pub use types::{ProcessedResult, PunctuationLabel, WordEntry, WordGroup};

/// The main processor.  Holds a configured jieba instance ready for reuse.
pub struct Processor {
    #[cfg(feature = "jieba")]
    jieba: jieba_rs::Jieba,
    config: Config,
}

impl Processor {
    /// Create a new processor, loading all configured dictionaries.
    ///
    /// When the `jieba` feature is disabled, this still succeeds but
    /// will only run the pure-acoustic (Tier C) pipeline.
    pub fn new(config: Config) -> Result<Self, DictError> {
        #[cfg(feature = "jieba")]
        {
            let mut jieba = jieba_rs::Jieba::new();
            dict::load_dicts(&mut jieba, &config.dict_paths)?;
            Ok(Self { jieba, config })
        }
        #[cfg(not(feature = "jieba"))]
        {
            Ok(Self { config })
        }
    }

    /// Run the CJK post-processing pipeline with per-layer feature control.
    ///
    /// Returns `None` if the language is not Chinese (not `zh-*`) or
    /// there are fewer than 2 words (nothing to group).
    pub fn process(&self, words: &[WordEntry], language: &str) -> Option<ProcessedResult> {
        if !language.starts_with("zh") || words.len() < 2 {
            return None;
        }

        let features = &self.config.features;

        // Layer 1: Gap Punctuation
        let punctuation = if features.enable_punctuation {
            layer1::detect_punctuation(words, &self.config)
        } else {
            vec![]
        };
        let punct_positions: std::collections::HashSet<usize> =
            punctuation.iter().map(|p| p.position).collect();

        // Layer 2 + 3: Segmentation
        #[cfg(feature = "jieba")]
        let final_groups = if features.enable_jieba {
            let jieba_groups = layer2::jieba_segment(words, &self.jieba);
            if features.enable_acoustic_merge {
                layer3::acoustic_verify(words, &jieba_groups, &self.config, &punct_positions)
            } else {
                jieba_groups
            }
        } else if features.enable_acoustic_merge {
            layer3::acoustic_only(words, &self.config)
        } else {
            identity_groups(words)
        };

        #[cfg(not(feature = "jieba"))]
        let final_groups = if features.enable_acoustic_merge {
            layer3::acoustic_only(words, &self.config)
        } else {
            identity_groups(words)
        };

        // Layer 4: Output Reconstruction
        Some(layer4::build_output(words, &final_groups, &punctuation))
    }
}

fn identity_groups(words: &[WordEntry]) -> Vec<(usize, usize, String)> {
    words
        .iter()
        .enumerate()
        .map(|(i, w)| (i, i + 1, w.char.clone()))
        .collect()
}
