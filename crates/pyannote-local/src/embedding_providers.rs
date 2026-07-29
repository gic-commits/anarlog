// EmbeddingProvider trait + implementations for Wespeaker/CAM++ ONNX models
//
// All models use fbank80 + per-utterance CMN as input frontend.
// Tensor names are auto-detected via probe_model() at construction time.

use hypr_onnx::{
    ndarray::{Array2, Array3, Axis},
    ort::{self, session::Session, value::TensorRef},
};

const NAME_SETS: &[(&str, &str)] = &[
    ("feats", "embs"),
    ("input_features", "embedding"),
    ("input", "embedding"),
    ("x", "embedding"),
];

pub trait EmbeddingProvider: Send {
    fn compute(&mut self, samples_f32: &[f32]) -> Result<Option<Vec<f32>>, crate::Error>;
    fn embedding_dim(&self) -> usize;
    fn name(&self) -> &str;
}

fn compute_fbank_cmn(samples_f32: &[f32]) -> Option<Array2<f32>> {
    let scaled: Vec<f32> = samples_f32.iter().map(|&s| s * 32768.0).collect();
    let features_knf = knf_rs::compute_fbank(&scaled).ok()?;
    let shape = features_knf.shape().to_vec();
    if shape.is_empty() || shape[0] == 0 {
        return None;
    }
    let mut features: Array2<f32> =
        Array2::from_shape_vec((shape[0], shape[1]), features_knf.iter().copied().collect())
            .ok()?;
    let mean = features.mean_axis(Axis(0))?;
    for mut row in features.rows_mut() {
        for (v, &m) in row.iter_mut().zip(mean.iter()) {
            *v -= m;
        }
    }
    Some(features)
}

fn probe_model(
    input_name: &str,
    output_name: &str,
    session: &mut Session,
) -> bool {
    let test_f32 = vec![0.0f32; 16000];
    if let Some(features) = compute_fbank_cmn(&test_f32) {
        let feats: Array3<f32> = features.insert_axis(Axis(0));
        if let Ok(tensor) = TensorRef::from_array_view(feats.view()) {
            let inputs = if input_name == "feats" {
                ort::inputs!["feats" => tensor]
            } else if input_name == "input_features" {
                ort::inputs!["input_features" => tensor]
            } else {
                ort::inputs!["input" => tensor]
            };
            if let Ok(outputs) = session.run(inputs) {
                return outputs.get(output_name).is_some();
            }
        }
    }
    false
}

fn embed_fbank_cmn(
    samples_f32: &[f32],
    session: &mut Session,
    input_name: &str,
    output_name: &str,
) -> Option<Vec<f32>> {
    let features = compute_fbank_cmn(samples_f32)?;
    let feats: Array3<f32> = features.insert_axis(Axis(0));
    let tensor = TensorRef::from_array_view(feats.view()).ok()?;
    let inputs = if input_name == "feats" {
        ort::inputs!["feats" => tensor]
    } else if input_name == "input_features" {
        ort::inputs!["input_features" => tensor]
    } else {
        ort::inputs!["input" => tensor]
    };
    let outputs = session.run(inputs).ok()?;
    let out = outputs.get(output_name)?;
    let arr = out.try_extract_array::<f32>().ok()?;
    let embs: Vec<f32> = arr.iter().copied().collect();
    if embs.is_empty() || !embs.iter().all(|v| v.is_finite()) {
        None
    } else {
        Some(embs)
    }
}

/// Shared data for fbank-based embedding models
pub struct FbankEmbedding {
    session: Session,
    input_name: &'static str,
    output_name: &'static str,
    dim: usize,
    model_name: String,
}

impl FbankEmbedding {
    pub fn new(
        model_path: &std::path::Path,
        model_name: String,
    ) -> Result<Self, crate::Error> {
        let mut session = hypr_onnx::load_model_from_path(model_path)?;

        let (inp, out) = NAME_SETS
            .iter()
            .find(|(inp, out)| probe_model(inp, out, &mut session))
            .map(|&(i, o)| (i, o))
            .unwrap_or_else(|| {
                eprintln!("[pyannote-local] WARNING: could not find tensor names for {model_name}, using feats/embs");
                ("feats", "embs")
            });

        // Determine dim by running a dummy pass
        let test_emb = embed_fbank_cmn(&vec![0.0f32; 16000], &mut session, inp, out)
            .unwrap_or_default();
        let dim = test_emb.len();

        Ok(Self {
            session,
            input_name: inp,
            output_name: out,
            dim,
            model_name,
        })
    }
}

impl EmbeddingProvider for FbankEmbedding {
    fn compute(&mut self, samples_f32: &[f32]) -> Result<Option<Vec<f32>>, crate::Error> {
        Ok(embed_fbank_cmn(samples_f32, &mut self.session, self.input_name, self.output_name))
    }

    fn embedding_dim(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        &self.model_name
    }
}

/// Auto-detect which provider to use based on file path
pub fn create_provider_from_path(
    model_path: &std::path::Path,
) -> Result<Box<dyn EmbeddingProvider>, crate::Error> {
    let name = model_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    FbankEmbedding::new(model_path, name.to_string()).map(|e| Box::new(e) as Box<dyn EmbeddingProvider>)
}
