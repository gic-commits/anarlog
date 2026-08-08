mod error;
pub use error::*;

use ort::{
    Result,
    session::{Session, builder::GraphOptimizationLevel},
};

pub use ndarray;
pub use ort;

pub fn load_model_from_bytes(bytes: &[u8]) -> Result<Session, Error> {
    load_model_from_bytes_with_threads(bytes, 1, 1)
}

pub fn load_model_from_path(path: impl AsRef<std::path::Path>) -> Result<Session, Error> {
    let bytes = std::fs::read(path)?;
    load_model_from_bytes(&bytes)
}

/// Load an ONNX session with explicit intra/inter thread counts. Some models
/// (e.g. the Wespeaker embedding network) benefit from >1 intra thread on CPU,
/// while others regress — so callers choose per model.
pub fn load_model_from_bytes_with_threads(
    bytes: &[u8],
    intra_threads: usize,
    inter_threads: usize,
) -> Result<Session, Error> {
    Ok(Session::builder()?
        .with_intra_threads(intra_threads)?
        .with_inter_threads(inter_threads)?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .commit_from_memory(bytes)?)
}
