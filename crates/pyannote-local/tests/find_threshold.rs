use pyannote_local::embedding_providers::create_provider_from_path;
use pyannote_local::incremental_diarization::{
    IncrementalDiarizationConfig, IncrementalDiarizationEngine,
};
use pyannote_local::segmentation::Segmenter;
use std::collections::HashSet;
use std::path::Path;

use rodio::Source;

fn load_pcm_from_mp3(path: &Path) -> Vec<i16> {
    let file = std::fs::File::open(path).unwrap();
    let source = rodio::Decoder::try_from(file).unwrap();
    let src_sr: u32 = source.sample_rate().into();
    let channels = source.channels().get() as usize;
    let f32_samples: Vec<f32> = source.collect();

    // Downmix stereo to mono (the audio files are 16kHz stereo mp3).
    let mono: Vec<f32> = if channels > 1 {
        f32_samples
            .chunks(channels)
            .map(|c| c.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        f32_samples
    };

    // resample to 16000
    let target_sr = 16000u32;
    let pcm = if src_sr == target_sr {
        mono
    } else {
        let ratio = src_sr as f64 / target_sr as f64;
        let out_len = (mono.len() as f64 / ratio).round() as usize;
        let last = mono.len().saturating_sub(1);
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let pos = i as f64 * ratio;
            let lo = (pos.floor() as usize).min(last);
            let hi = (pos.ceil() as usize).min(last);
            let frac = pos - lo as f64;
            out.push(mono[lo] * (1.0 - frac as f32) + mono[hi] * frac as f32);
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
            ..Default::default()
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

/// Regression: the legacy 0.85 default collapsed this session's multi-speaker
/// audio to a single speaker. The current default threshold (0.5) combined
/// with the contiguous-span speaker filter must recover multiple speakers
/// through the real feed_segments + finalize path.
#[test]
fn adaptive_threshold_recovers_multiple_speakers() {
    let audio_path = Path::new(
        "/Users/fuqingqiu/Library/Application Support/com.hyprnote.dev/sessions/5fdd76a7-a324-4eda-b399-415dd70503d3/audio.mp3",
    );
    if !audio_path.exists() {
        eprintln!("audio not found, skipping adaptive threshold regression");
        return;
    }

    let pcm = load_pcm_from_mp3(audio_path);
    let mut segmenter = Segmenter::new(16000).unwrap();
    let segments = segmenter.process(&pcm, 16000).unwrap();

    let model_name = "wespeaker_zh_cnceleb_resnet34_LM.onnx";
    let model_path = resolve_model_path(model_name).unwrap();

    // Current default threshold 0.5 — separates speakers better than the
    // legacy 0.85 (which collapsed multi-speaker audio to one cluster).
    let mut engine = IncrementalDiarizationEngine::new(IncrementalDiarizationConfig {
        sample_rate: 16000,
        model_path: Some(model_path.to_str().unwrap().to_string()),
        threshold: 0.5,
        recluster_interval: usize::MAX,
        ..Default::default()
    })
    .unwrap();
    engine.feed_segments(&segments).unwrap();
    engine.finalize();

    let segs = engine.speaker_segments();
    let unique: HashSet<usize> = segs.iter().map(|s| s.speaker).collect();
    eprintln!(
        "threshold 0.5: {} segs, {} unique speakers {:?}",
        segs.len(),
        unique.len(),
        unique.iter().copied().collect::<Vec<_>>(),
    );
    assert!(
        unique.len() >= 3,
        "threshold 0.5 should recover >=3 speakers, got {}",
        unique.len()
    );
}
