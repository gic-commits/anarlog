use std::sync::Arc;

use tokio::sync::mpsc;

use crate::events::*;

#[derive(Debug, Clone)]
pub struct ProgressiveBatchParams {
    pub session_id: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub language: Option<String>,
    pub sample_rate: u32,
    pub diarization_enabled: bool,
    pub diarization_model: Option<String>,
    pub diarization_threshold: f32,
}

pub trait ListenerRuntime: hypr_storage::StorageRuntime {
    fn emit_lifecycle(&self, event: SessionLifecycleEvent);
    fn emit_progress(&self, event: SessionProgressEvent);
    fn emit_error(&self, event: SessionErrorEvent);
    fn emit_data(&self, event: SessionDataEvent);

    /// Start consuming PCM frames for a progressive batch session.
    /// The runtime should spawn a task that reads from `pcm_rx` and feeds
    /// them to a ProgressiveBatchManager.
    /// Default impl drops both params and rx (no-op for test runtimes).
    fn start_progressive_batch_stream(
        &self,
        _params: ProgressiveBatchParams,
        _pcm_rx: mpsc::Receiver<Arc<[f32]>>,
    ) {
    }
}
