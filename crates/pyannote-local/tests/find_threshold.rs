use pyannote_local::embedding_providers::create_provider_from_path;
use pyannote_local::incremental_diarization::{
    IncrementalDiarizationConfig, IncrementalDiarizationEngine,
};
use std::collections::HashSet;
use std::path::Path;

use rodio::Source;

fn load_pcm_from_mp3(path: &Path) -> Vec<i16> {
    let file = std::fs::File::open(path).unwrap();
    let source = rodio::Decoder::try_from(file).unwrap();
    let src_sr: u32 = source.sample_rate().into();
    let f32_samples: Vec<f32> = source.collect();

    // resample to 16000
    let target_sr = 16000u32;
    let pcm = if src_sr == target_sr {
        f32_samples
    } else {
        let ratio = src_sr as f64 / target_sr as f64;
        let out_len = (f32_samples.len() as f64 / ratio).round() as usize;
        let last = f32_samples.len().saturating_sub(1);
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let pos = i as f64 * ratio;
            let lo = (pos.floor() as usize).min(last);
            let hi = (pos.ceil() as usize).min(last);
            let frac = pos - lo as f64;
            out.push(f32_samples[lo] * (1.0 - frac as f32) + f32_samples[hi] * frac as f32);
        }
        out
    };

    pcm.into_iter().map(|s| (s * 32768.0) as i16).collect()
}

fn resolve_model_path(path_str: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path_str);
    if p.exists() {
        return Some(p.to_path_buf());
    }
    let test_path =
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/models")).join(path_str);
    if test_path.exists() {
        return Some(test_path);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_path = cwd.join("models").join(path_str);
        if cwd_path.exists() {
            return Some(cwd_path);
        }
    }
    None
}

#[test]
fn sweep_threshold_for_audio() {
    let audio_path = Path::new(
        "/Users/fuqingqiu/Library/Application Support/com.hyprnote.dev/sessions/c5ee333b-9761-4705-9862-d3f483d49d69/audio.mp3",
    );
    assert!(
        audio_path.exists(),
        "audio file not found at {:?}",
        audio_path
    );

    let pcm = load_pcm_from_mp3(audio_path);
    eprintln!(
        "loaded {} PCM samples ({:.1}s at 16kHz)",
        pcm.len(),
        pcm.len() as f64 / 16000.0
    );

    let model_name = "wespeaker_zh_cnceleb_resnet34_LM.onnx";
    let model_path = resolve_model_path(model_name);
    assert!(model_path.is_some(), "model '{}' not found", model_name);
    let model_path = model_path.unwrap();
    eprintln!("model path: {:?}", model_path);

    // Verify model loads
    let provider = create_provider_from_path(&model_path);
    assert!(
        provider.is_ok(),
        "failed to load model: {:?}",
        provider.err()
    );
    eprintln!(
        "model loaded successfully, dim={}",
        provider.unwrap().embedding_dim()
    );

    // Sweep thresholds
    let thresholds: Vec<f32> = (50..=99).map(|i| i as f32 / 100.0).collect();

    eprintln!("\n--- threshold sweep ---");
    for &t in &thresholds {
        let mut engine = IncrementalDiarizationEngine::new(IncrementalDiarizationConfig {
            sample_rate: 16000,
            model_path: Some(model_name.to_string()),
            threshold: t,
            recluster_interval: usize::MAX,
        })
        .unwrap();

        engine.feed_pcm(&pcm).unwrap();
        engine.finalize();

        let segs = engine.speaker_segments();
        let unique: HashSet<usize> = segs.iter().map(|s| s.speaker).collect();
        let valid_count = segs.iter().filter(|s| s.embedding_valid).count();
        eprintln!(
            "  threshold={:.2}: {} segs ({} valid), {} unique speakers {:?}",
            t,
            segs.len(),
            valid_count,
            unique.len(),
            unique.iter().copied().collect::<Vec<_>>(),
        );
    }
}
