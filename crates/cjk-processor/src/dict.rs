use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DictError {
    #[error("failed to load dictionary at {path}: {detail}")]
    LoadError { path: String, detail: String },

    #[error("io error at {path}: {detail}")]
    IoError { path: String, detail: String },

    #[error("jieba not available (feature not enabled)")]
    JiebaNotAvailable,
}

impl From<jieba_rs::Error> for DictError {
    fn from(e: jieba_rs::Error) -> Self {
        DictError::LoadError {
            path: String::new(),
            detail: e.to_string(),
        }
    }
}

/// Load supplementary dictionaries into a jieba instance.
///
/// Each path is loaded via `jieba.load_dict()`.  Missing files are
/// silently skipped (to allow graceful degradation when optional dicts
/// haven't been downloaded yet).
#[cfg(feature = "jieba")]
pub fn load_dicts(
    jieba: &mut jieba_rs::Jieba,
    paths: &[impl AsRef<Path>],
) -> Result<usize, DictError> {
    let mut loaded = 0;
    for path in paths {
        let p = path.as_ref();
        if !p.exists() {
            continue;
        }
        let file = File::open(p).map_err(|e| DictError::IoError {
            path: p.display().to_string(),
            detail: e.to_string(),
        })?;
        let mut reader = BufReader::new(file);
        jieba.load_dict(&mut reader)?;
        loaded += 1;
    }
    Ok(loaded)
}

#[cfg(not(feature = "jieba"))]
pub fn load_dicts(_jieba: &mut (), _paths: &[impl AsRef<Path>]) -> Result<usize, DictError> {
    Err(DictError::JiebaNotAvailable)
}
