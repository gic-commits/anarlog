use std::collections::HashSet;
use std::path::PathBuf;

/// Per-layer feature flags for the CJK post-processing pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CjkFeatures {
    /// Layer 1: Insert punctuation (。，) based on acoustic gaps.
    pub enable_punctuation: bool,
    /// Layer 2: Jieba lexical segmentation.
    pub enable_jieba: bool,
    /// Layer 3: Acoustic merge.  With jieba enabled, verifies jieba groupings;
    /// without jieba, runs pure-acoustic OOV merge.
    pub enable_acoustic_merge: bool,
}

impl Default for CjkFeatures {
    fn default() -> Self {
        Self {
            enable_punctuation: true,
            enable_jieba: cfg!(feature = "jieba"),
            enable_acoustic_merge: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Supplementary dictionary paths loaded via jieba.load_userdict()
    pub dict_paths: Vec<PathBuf>,

    /// Minimum gap (seconds) to insert a period (句号)
    pub sentence_gap: f64,

    /// Minimum gap (seconds) to insert a comma (逗号)
    pub clause_gap: f64,

    /// Acoustic ratio threshold: r < alpha_low → same word
    pub alpha_low: f64,

    /// Acoustic ratio threshold: r > alpha_high → new word
    pub alpha_high: f64,

    /// Sliding window size for median-based acoustic detection (odd number)
    pub window_size: usize,

    /// Per-layer feature flags.
    pub features: CjkFeatures,

    /// Set of single-character function words that should never be merged.
    pub func_words: HashSet<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dict_paths: Vec::new(),
            sentence_gap: 0.80,
            clause_gap: 0.25,
            alpha_low: 0.60,
            alpha_high: 1.50,
            window_size: 5,
            features: CjkFeatures::default(),
            func_words: default_func_words(),
        }
    }
}

pub fn default_func_words() -> HashSet<String> {
    [
        "的", "了", "在", "中", "和", "于", "之", "等", "由", "其", "被", "向", "以", "与", "而",
        "或", "但", "是", "有", "不", "也", "就", "这", "那", "都", "还", "很", "更", "将", "把",
        "从", "对", "为", "上", "下", "到", "让", "给", "用", "能", "会", "要",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}
