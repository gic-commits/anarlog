#[derive(Debug, Clone)]
pub struct WordEntry {
    pub char: String,
    pub start: f64,
    pub end: f64,
}

impl WordEntry {
    pub fn new(char: String, start: f64, end: f64) -> Self {
        Self { char, start, end }
    }

    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

#[derive(Debug, Clone)]
pub struct WordGroup {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub chars: Vec<WordEntry>,
}

#[derive(Debug, Clone)]
pub struct PunctuationLabel {
    pub position: usize,
    pub punct: String,
}

#[derive(Debug, Clone)]
pub struct ProcessedResult {
    pub text: String,
    pub word_groups: Vec<WordGroup>,
    pub punctuation: Vec<PunctuationLabel>,
    pub segment_boundaries: Vec<usize>,
}
