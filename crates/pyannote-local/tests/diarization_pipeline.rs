// 裸测代码：直接调用本地 ONNX 说话人分离管线
// 验证不同说话人区分度 + 合成多说话人 + 真实韩语/英语多说话人
// 含模型对比：pyannote-local (512-dim) vs hypr-embedding (256-dim) vs Wespeaker ResNet34 (256-dim)

use pyannote_local::{
    embedding::EmbeddingExtractor as PyannoteExtractor,
    segmentation::{Segment, Segmenter},
};

use hypr_onnx::ndarray::{Array2, Array3, Axis};
use hypr_onnx::ort;
use hypr_onnx::ort::session::Session;

use rodio::Source;
use std::path::Path;

// ---------------------------------------------------------------------------
// Cosine distance helper (manual computation for robustness)
// ---------------------------------------------------------------------------
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denom = na * nb;
    if denom == 0.0 {
        return 1.0;
    }
    1.0 - (dot / denom).clamp(-1.0, 1.0)
}

// ---------------------------------------------------------------------------
// 重采样 (nearest-neighbor, 仅用于验证测试)
// ---------------------------------------------------------------------------

fn resample_f32(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate {
        return samples.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = (samples.len() as f64 / ratio).round() as usize;
    let last = samples.len().saturating_sub(1);
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let lo = (pos.floor() as usize).min(last);
        let hi = (pos.ceil() as usize).min(last);
        let frac = pos - lo as f64;
        out.push(samples[lo] * (1.0 - frac as f32) + samples[hi] * frac as f32);
    }
    out
}

// ---------------------------------------------------------------------------
// 音频加载工具
// ---------------------------------------------------------------------------

fn load_audio_f32(path: &Path, target_sr: u32) -> (Vec<f32>, u32) {
    let file = std::fs::File::open(path).unwrap();
    let source = rodio::Decoder::try_from(file).unwrap();
    let src_sr: u32 = source.sample_rate().into();
    let samples: Vec<f32> = source.collect();
    if src_sr != target_sr {
        (resample_f32(&samples, src_sr, target_sr), target_sr)
    } else {
        (samples, src_sr)
    }
}

fn f32_to_i16(f32_samples: &[f32]) -> Vec<i16> {
    f32_samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

fn load_audio_i16(path: &Path, target_sr: u32) -> (Vec<i16>, u32) {
    let (f32_samples, sr) = load_audio_f32(path, target_sr);
    (f32_to_i16(&f32_samples), sr)
}

fn wav_bytes_to_i16_s16le(bytes: &[u8]) -> Vec<i16> {
    // Strip WAV header (44 bytes) and decode as s16le
    let data = if bytes.len() > 44 {
        &bytes[44..]
    } else {
        bytes
    };
    data.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

// ---------------------------------------------------------------------------
// 聚类 (agglomerative average-linkage)
// ---------------------------------------------------------------------------

fn agglomerative_cluster(dist: &[f32], n: usize, threshold: f32) -> Vec<usize> {
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![0];
    }

    let mut clusters: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();

    loop {
        let mut min_dist = f32::MAX;
        let mut merge = None;

        for i in 0..clusters.len() {
            for j in i + 1..clusters.len() {
                let mut total = 0.0f64;
                let mut count = 0usize;
                for &a in &clusters[i] {
                    for &b in &clusters[j] {
                        total += dist[a * n + b] as f64;
                        count += 1;
                    }
                }
                let avg = (total / count as f64) as f32;
                if avg < min_dist {
                    min_dist = avg;
                    merge = Some((i, j));
                }
            }
        }

        match merge {
            Some((i, j)) if min_dist < threshold => {
                let removed = clusters.remove(j);
                clusters[i].extend(removed);
            }
            _ => break,
        }
    }

    let mut assignments = vec![0usize; n];
    for (cid, members) in clusters.iter().enumerate() {
        for &idx in members {
            assignments[idx] = cid;
        }
    }
    assignments
}

// ---------------------------------------------------------------------------
// 提取所有 segment 的 embedding（处理 TooShort 错误）
// ---------------------------------------------------------------------------

fn extract_embeddings(
    extractor: &mut PyannoteExtractor,
    segments: &[Segment],
) -> Vec<Option<Vec<f32>>> {
    segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            extractor.compute(f32_s.into_iter()).ok()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 打印结构化结果
// ---------------------------------------------------------------------------

fn print_results(label: &str, segments: &[&Segment], assignments: &[usize]) {
    println!("\n=== {label} ===");
    println!(
        "{:<4} {:<12} {:<12} {:<10} {}",
        "#", "start(s)", "end(s)", "dur(s)", "speaker"
    );
    for (i, seg) in segments.iter().enumerate() {
        let speaker = if i < assignments.len() {
            format!("Speaker_{}", assignments[i])
        } else {
            "N/A".into()
        };
        println!(
            "{:<4} {:<12.3} {:<12.3} {:<10.3} {}",
            i,
            seg.start,
            seg.end,
            seg.end - seg.start,
            speaker,
        );
    }
}

// ===========================================================================
// Test 1: Speaker Discrimination
// ===========================================================================

#[test]
fn test_speaker_discrimination() {
    const SR: u32 = 16000;
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data");

    let female = load_audio_f32(&base.join("female_welcome_1.mp3"), SR).0;
    let male_1 = load_audio_f32(&base.join("male_welcome_1.mp3"), SR).0;
    let male_2 = load_audio_f32(&base.join("male_welcome_2.mp3"), SR).0;

    let mut ext = PyannoteExtractor::new();
    let emb_f = ext.compute(female.into_iter()).unwrap();
    let emb_m1 = ext.compute(male_1.into_iter()).unwrap();
    let emb_m2 = ext.compute(male_2.into_iter()).unwrap();

    let d_m1m2 = cosine_distance(&emb_m1, &emb_m2);
    let d_fm1 = cosine_distance(&emb_f, &emb_m1);
    let d_fm2 = cosine_distance(&emb_f, &emb_m2);

    println!("=== 1) Speaker Discrimination ===");
    println!("embedding dim:  {}", emb_f.len());
    println!(
        "male_1 (2.4s) vs male_2 (1.6s)  (same label): {:.6}",
        d_m1m2
    );
    println!("female (2.4s)  vs male_1 (2.4s) (different):  {:.6}", d_fm1);
    println!("female (2.4s)  vs male_2 (1.6s) (different):  {:.6}", d_fm2);
    println!("---");
    println!("same-speaker distance:  {:.4} (male_1 <-> male_2)", d_m1m2);
    println!("cross-speaker distance: {:.4} (female <-> male_1)", d_fm1);
    println!("cross-speaker distance: {:.4} (female <-> male_2)", d_fm2);
    println!(
        "ratio (worst cross/same): {:.1}x",
        d_fm1.min(d_fm2) / d_m1m2.min(0.001)
    );
    println!("NOTE: male_2 is only 1.6s — may be too short for reliable embedding");
}

// ===========================================================================
// Test 2: Synthetic multi-speaker (F - M - F)
// ===========================================================================

#[test]
fn test_synthetic_multispeaker() {
    const SR: u32 = 16000;
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data");

    let (female_i16, _) = load_audio_i16(&base.join("female_welcome_1.mp3"), SR);
    let (male_i16, _) = load_audio_i16(&base.join("male_welcome_1.mp3"), SR);
    let silence = vec![0i16; (SR * 2) as usize]; // 2s gap

    let multi: Vec<i16> = [
        female_i16.clone(),
        silence.clone(),
        male_i16.clone(),
        silence.clone(),
        female_i16.clone(),
    ]
    .concat();

    let mut seg = Segmenter::new(SR).unwrap();
    let segments = seg.process(&multi, SR).unwrap();

    println!("\n=== 2) Synthetic Multi-Speaker (F-M-F) ===");
    println!("total audio: {:.2}s", multi.len() as f64 / SR as f64);
    println!("segments found: {}", segments.len());

    if segments.len() < 3 {
        println!("WARNING: <3 segments, can't verify clustering (audio too short for model?)");
        // 仍然提取得分
    }

    let mut ext = PyannoteExtractor::new();
    let embeddings = extract_embeddings(&mut ext, &segments);

    // 过滤出 successful embeddings
    let valid: Vec<(usize, &[f32])> = embeddings
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
        .collect();

    if valid.len() < 2 {
        println!(
            "NOT ENOUGH valid embeddings (<2), skipping distance/clustering (likely all too short)"
        );
        print_results(
            "2) Synthetic (raw)",
            &segments.iter().collect::<Vec<_>>(),
            &[],
        );
        return;
    }

    // Print embedding stats to check for NaN/all-zero
    for &(orig_idx, emb) in &valid {
        let mean = emb.iter().sum::<f32>() / emb.len() as f32;
        let max = emb.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let min = emb.iter().cloned().fold(f32::INFINITY, f32::min);
        let nan = emb.iter().filter(|&&v| v.is_nan()).count();
        println!(
            "  emb[{}]: dim={} mean={:.6} range=[{:.6},{:.6}] nan={}",
            orig_idx,
            emb.len(),
            mean,
            min,
            max,
            nan
        );
    }

    // Build distance matrix for valid segments only
    let n = valid.len();
    let mut dist = vec![0.0f32; n * n];
    for i in 0..n {
        for j in 0..n {
            dist[i * n + j] = cosine_distance(valid[i].1, valid[j].1);
        }
    }

    println!("\nDistance matrix (valid segments only):");
    print!("         ");
    for i in 0..n {
        print!("  s{}  ", valid[i].0);
    }
    println!();
    for i in 0..n {
        print!(" s{}:", valid[i].0);
        for j in 0..n {
            print!(" {:.4}", dist[i * n + j]);
        }
        println!();
    }

    let threshold = 0.45;
    let cluster_ids = agglomerative_cluster(&dist, n, threshold);

    // Map back to original segment indices
    let mut all_assignments = vec![usize::MAX; segments.len()];
    for (&(orig_idx, _), &cid) in valid.iter().zip(cluster_ids.iter()) {
        all_assignments[orig_idx] = cid;
    }

    print_results(
        "2) Synthetic (clustered)",
        &segments.iter().collect::<Vec<_>>(),
        &all_assignments,
    );

    // First and last valid segments should be same speaker (both female)
    if valid.len() >= 2 {
        let first = cluster_ids[0];
        let last = cluster_ids[valid.len() - 1];
        println!(
            "\nFirst vs last cluster: {} vs {} (same={})",
            first,
            last,
            first == last
        );
        // 不强制 assert，因为 segment 级别长短不一
        if first == last {
            println!("  ✓ Same speaker for first & last (both female)");
        } else {
            println!("  ✗ Different clusters (segments too short or model limitation)");
        }
    }
}

// ===========================================================================
// Test 3: Korean multi-speaker (korean_1, ~4.8min, 16kHz WAV)
// ===========================================================================

#[test]
fn test_korean_diarization() {
    const SR: u32 = 16000;
    let audio_i16 = wav_bytes_to_i16_s16le(hypr_data::korean_1::AUDIO);

    println!("\n=== 3) Korean Multi-Speaker (korean_1) ===");
    println!("duration: {:.2}s", audio_i16.len() as f64 / SR as f64);
    println!("samples: {}", audio_i16.len());

    let mut seg = Segmenter::new(SR).unwrap();
    let segments = seg.process(&audio_i16, SR).unwrap();
    println!("segments found: {}", segments.len());

    // 如果段太多，截取前 20 段
    let test_segments: Vec<&Segment> = segments.iter().take(20).collect();
    println!("using first {} segments for embedding", test_segments.len());

    let mut ext = PyannoteExtractor::new();
    let embeddings: Vec<Option<Vec<f32>>> = test_segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            ext.compute(f32_s.into_iter()).ok()
        })
        .collect();

    let valid: Vec<(usize, &[f32])> = embeddings
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
        .collect();

    println!("valid embeddings: {}/{}", valid.len(), test_segments.len());

    for &(orig_idx, emb) in &valid {
        let mean = emb.iter().sum::<f32>() / emb.len() as f32;
        let nz = emb.iter().filter(|&&v| v != 0.0).count();
        println!(
            "  emb[{}]: dim={} mean={:.4} nonzero={}/{}",
            orig_idx,
            emb.len(),
            mean,
            nz,
            emb.len()
        );
    }

    if valid.len() < 4 {
        println!("  Too few valid embeddings to cluster meaningfully");
        return;
    }

    let n = valid.len();
    let mut dist = vec![0.0f32; n * n];
    for i in 0..n {
        for j in 0..n {
            dist[i * n + j] = cosine_distance(valid[i].1, valid[j].1);
        }
    }

    println!("\nDistance matrix (first {}):", n.min(10));
    print!("         ");
    for i in 0..n.min(10) {
        print!("  s{}  ", valid[i].0);
    }
    println!();
    for i in 0..n.min(10) {
        print!(" s{}:", valid[i].0);
        for j in 0..n.min(10) {
            print!(" {:.4}", dist[i * n + j]);
        }
        println!();
    }

    println!("\nThreshold sweep:");
    for &threshold in &[0.02, 0.03, 0.05, 0.08, 0.12, 0.20, 0.35, 0.45, 0.55] {
        let cids = agglomerative_cluster(&dist, n, threshold);
        let nc = cids.iter().max().unwrap_or(&0) + 1;
        println!("  threshold={threshold:.2} → {nc} clusters");
    }

    let threshold = 0.45;
    let cluster_ids = agglomerative_cluster(&dist, n, threshold);
    let n_clusters = cluster_ids.iter().max().unwrap_or(&0) + 1;

    let mut all_assignments = vec![usize::MAX; test_segments.len()];
    for (&(orig_idx, _), &cid) in valid.iter().zip(cluster_ids.iter()) {
        all_assignments[orig_idx] = cid;
    }

    print_results(
        "3) Korean (first 20 segs)",
        &test_segments,
        &all_assignments,
    );
    println!(
        "\nKOREAN SUMMARY: {} segments → {} speakers (threshold={threshold})",
        valid.len(),
        n_clusters
    );
}

// ===========================================================================
// Test 4: English multi-speaker (english_10, ~15min)
// ===========================================================================

#[test]
fn test_english_diarization() {
    const SR: u32 = 16000;
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/data/src/english_10");

    // 只用开头 60s 避免测试过慢
    let (audio_f32, _) = load_audio_f32(&base.join("audio.mp3"), SR);
    let max_samples = (SR * 60) as usize;
    let clipped = if audio_f32.len() > max_samples {
        &audio_f32[..max_samples]
    } else {
        &audio_f32
    };
    let audio_i16 = f32_to_i16(clipped);

    println!("\n=== 4) English Multi-Speaker (english_10, first 60s) ===");
    println!("duration: {:.2}s", audio_i16.len() as f64 / SR as f64);

    let mut seg = Segmenter::new(SR).unwrap();
    let all_segments = seg.process(&audio_i16, SR).unwrap();
    println!("segments found: {}", all_segments.len());

    let test_segments: Vec<&Segment> = all_segments.iter().take(20).collect();

    let mut ext = PyannoteExtractor::new();
    let embeddings: Vec<Option<Vec<f32>>> = test_segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            ext.compute(f32_s.into_iter()).ok()
        })
        .collect();

    let valid: Vec<(usize, &[f32])> = embeddings
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
        .collect();

    println!("valid embeddings: {}/{}", valid.len(), test_segments.len());

    for &(orig_idx, emb) in &valid {
        let mean = emb.iter().sum::<f32>() / emb.len() as f32;
        let nz = emb.iter().filter(|&&v| v != 0.0).count();
        println!(
            "  emb[{}]: dim={} mean={:.4} nonzero={}/{}",
            orig_idx,
            emb.len(),
            mean,
            nz,
            emb.len()
        );
    }

    if valid.len() < 4 {
        println!("  Too few valid embeddings");
        return;
    }

    let n = valid.len();
    let mut dist = vec![0.0f32; n * n];
    for i in 0..n {
        for j in 0..n {
            dist[i * n + j] = cosine_distance(valid[i].1, valid[j].1);
        }
    }

    println!("\nDistance matrix (top-left {}):", n.min(8));
    print!("         ");
    for i in 0..n.min(8) {
        print!("  s{}  ", valid[i].0);
    }
    println!();
    for i in 0..n.min(8) {
        print!(" s{}:", valid[i].0);
        for j in 0..n.min(8) {
            print!(" {:.4}", dist[i * n + j]);
        }
        println!();
    }

    println!("\nThreshold sweep:");
    for &threshold in &[0.02, 0.03, 0.05, 0.08, 0.12, 0.20, 0.35, 0.45, 0.55] {
        let ids = agglomerative_cluster(&dist, n, threshold);
        let nc = ids.iter().max().unwrap_or(&0) + 1;
        println!("  threshold={threshold:.2} → {nc} clusters");
    }

    let threshold = 0.45;
    let cluster_ids = agglomerative_cluster(&dist, n, threshold);
    let n_clusters = cluster_ids.iter().max().unwrap_or(&0) + 1;

    let mut all_assignments = vec![usize::MAX; test_segments.len()];
    for (&(orig_idx, _), &cid) in valid.iter().zip(cluster_ids.iter()) {
        all_assignments[orig_idx] = cid;
    }

    print_results(
        "4) English (first 20 segs)",
        &test_segments,
        &all_assignments,
    );
    println!(
        "\nENGLISH SUMMARY: {} segments → {} speakers (threshold={threshold})",
        valid.len(),
        n_clusters
    );
}

// ===========================================================================
// Test 5: Model comparison — pyannote-local (512-dim) vs hypr-embedding (256-dim)
// Same synthetic F-M-F audio, both models, same clustering
// ===========================================================================

fn embed_with_hypr(
    extractor: &mut hypr_embedding::EmbeddingExtractor,
    segments: &[Segment],
) -> Vec<Option<Vec<f32>>> {
    segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            extractor.compute_optional(&f32_s).ok().flatten()
        })
        .collect()
}

fn run_comparison(
    label: &str,
    f32_audio: &[f32],
    sr: u32,
    pyannote_ext: &mut PyannoteExtractor,
    hypr_ext: &mut hypr_embedding::EmbeddingExtractor,
) {
    let audio_i16: Vec<i16> = f32_audio
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();

    let mut segmenter = Segmenter::new(sr).unwrap();
    let segments = segmenter.process(&audio_i16, sr).unwrap();

    println!("\n=== 5) Model Comparison: {label} ===");
    println!("segments: {}", segments.len());

    // pyannote-local (512-dim)
    let py_embs: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            pyannote_ext.compute(f32_s.into_iter()).ok()
        })
        .collect();
    // hypr-embedding (256-dim)
    let hy_embs = embed_with_hypr(hypr_ext, &segments);

    let py_valid: Vec<(usize, &[f32])> = py_embs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
        .collect();
    let hy_valid: Vec<(usize, &[f32])> = hy_embs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
        .collect();

    println!(
        "pyannote valid:  {}/{} (dim={})",
        py_valid.len(),
        segments.len(),
        py_valid.first().map(|(_, e)| e.len()).unwrap_or(0)
    );
    println!(
        "hypr-embedding valid: {}/{} (dim={})",
        hy_valid.len(),
        segments.len(),
        hy_valid.first().map(|(_, e)| e.len()).unwrap_or(0)
    );

    if py_valid.len() < 2 || hy_valid.len() < 2 {
        println!("  too few valid embeddings, skipping");
        return;
    }

    // Distance matrices
    let py_n = py_valid.len();
    let mut py_dist = vec![0.0f32; py_n * py_n];
    for i in 0..py_n {
        for j in 0..py_n {
            py_dist[i * py_n + j] = cosine_distance(py_valid[i].1, py_valid[j].1);
        }
    }

    let hy_n = hy_valid.len();
    let mut hy_dist = vec![0.0f32; hy_n * hy_n];
    for i in 0..hy_n {
        for j in 0..hy_n {
            hy_dist[i * hy_n + j] = cosine_distance(hy_valid[i].1, hy_valid[j].1);
        }
    }

    // Print distance matrices side by side
    println!("\nDistance matrices (pyannote on left, hypr on right):");
    for i in 0..py_n.min(hy_n).min(6) {
        print!(" s{:<3} |", py_valid[i].0);
        for j in 0..py_n.min(6) {
            print!(" {:.4}", py_dist[i * py_n + j]);
        }
        print!("  ||  s{:<3} |", hy_valid[i].0);
        let hi = hy_valid
            .iter()
            .position(|&(idx, _)| idx == py_valid[i].0)
            .unwrap_or(i);
        for j in 0..hy_n.min(6) {
            let hj = hy_valid
                .iter()
                .position(|&(idx, _)| idx == py_valid[j].0)
                .unwrap_or(j);
            print!(" {:.4}", hy_dist[hi * hy_n + hj]);
        }
        println!();
    }

    // Threshold sweep
    println!("\nThreshold sweep:");
    for &t in &[0.02, 0.03, 0.05, 0.08, 0.12, 0.20, 0.35, 0.45] {
        let pc = agglomerative_cluster(&py_dist, py_n, t)
            .iter()
            .max()
            .unwrap_or(&0)
            + 1;
        let hc = agglomerative_cluster(&hy_dist, hy_n, t)
            .iter()
            .max()
            .unwrap_or(&0)
            + 1;
        println!("  t={t:.2}: pyannote={pc:2}  hypr={hc:2}");
    }

    // Intra-speaker vs inter-speaker stats
    if py_valid.len() >= 2 && hy_valid.len() >= 2 {
        let mut py_same = Vec::new();
        let mut py_diff = Vec::new();
        let mut hy_same = Vec::new();
        let mut hy_diff = Vec::new();
        for i in 0..py_n {
            for j in i + 1..py_n {
                let d = py_dist[i * py_n + j];
                if d < 0.02 {
                    py_same.push(d);
                } else {
                    py_diff.push(d);
                }
            }
        }
        for i in 0..hy_n {
            for j in i + 1..hy_n {
                let d = hy_dist[i * hy_n + j];
                if d < 0.02 {
                    hy_same.push(d);
                } else {
                    hy_diff.push(d);
                }
            }
        }
        let py_same_avg = if py_same.is_empty() {
            0.0
        } else {
            py_same.iter().sum::<f32>() / py_same.len() as f32
        };
        let py_diff_avg = if py_diff.is_empty() {
            0.0
        } else {
            py_diff.iter().sum::<f32>() / py_diff.len() as f32
        };
        let hy_same_avg = if hy_same.is_empty() {
            0.0
        } else {
            hy_same.iter().sum::<f32>() / hy_same.len() as f32
        };
        let hy_diff_avg = if hy_diff.is_empty() {
            0.0
        } else {
            hy_diff.iter().sum::<f32>() / hy_diff.len() as f32
        };
        println!("\nStats (auto threshold 0.02 for same/diff split):");
        println!(
            "  pyannote:  same_avg={:.4} diff_avg={:.4} ratio={:.1}x",
            py_same_avg,
            py_diff_avg,
            py_diff_avg / py_same_avg.max(0.001)
        );
        println!(
            "  hypr:      same_avg={:.4} diff_avg={:.4} ratio={:.1}x",
            hy_same_avg,
            hy_diff_avg,
            hy_diff_avg / hy_same_avg.max(0.001)
        );
    }
}

#[test]
fn test_model_comparison_discrimination() {
    const SR: u32 = 16000;
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data");

    // Female + Male 1 as different speakers
    let (female, _) = load_audio_f32(&base.join("female_welcome_1.mp3"), SR);
    let (male_1, _) = load_audio_f32(&base.join("male_welcome_1.mp3"), SR);

    // Build F-M concatenation
    let silence = vec![0.0f32; SR as usize];
    let combined: Vec<f32> = [female.clone(), silence.clone(), male_1.clone()].concat();

    let mut py_ext = PyannoteExtractor::new();
    let mut hy_ext = hypr_embedding::EmbeddingExtractor::new().unwrap();
    run_comparison("F-M concatenation", &combined, SR, &mut py_ext, &mut hy_ext);
}

#[test]
fn test_model_comparison_korean() {
    const SR: u32 = 16000;
    let audio_i16 = wav_bytes_to_i16_s16le(hypr_data::korean_1::AUDIO);
    let audio_f32: Vec<f32> = audio_i16.iter().map(|&s| s as f32 / 32768.0).collect();

    // Use first 30s for faster test
    let max_s = (SR * 30) as usize;
    let clip = if audio_f32.len() > max_s {
        &audio_f32[..max_s]
    } else {
        &audio_f32
    };

    let mut py_ext = PyannoteExtractor::new();
    let mut hy_ext = hypr_embedding::EmbeddingExtractor::new().unwrap();
    run_comparison("korean_1 (first 30s)", clip, SR, &mut py_ext, &mut hy_ext);
}

// ===========================================================================
// Model comparison — Wespeaker ResNet34 vs hypr-embedding
// Wespeaker: fbank80 + per-utterance CMN. Input (1,T,80), Output (1,256).
// VoxCeleb model uses feats/embs; CN-Celeb uses input_features/embedding.
// ===========================================================================

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

const MODELS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/models");

fn probe_model(input_name: &str, output_name: &str, session: &mut Session) -> bool {
    use hypr_onnx::ort::value::TensorRef;
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

fn embed_ws_seg(
    seg: &Segment,
    session: &mut Session,
    input_name: &str,
    output_name: &str,
) -> Option<Vec<f32>> {
    use hypr_onnx::ort::value::TensorRef;
    let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
    let features = compute_fbank_cmn(&f32_s)?;
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

const NAME_SETS: &[(&str, &str)] = &[
    ("feats", "embs"),
    ("input_features", "embedding"),
    ("input", "embedding"),
    ("x", "embedding"),
];

fn run_wespeaker_fm_test(model_filename: &str, model_label: &str, _voxceleb_names: bool) {
    const SR: u32 = 16000;
    let model_path = std::path::Path::new(MODELS_DIR).join(model_filename);
    if !model_path.exists() {
        println!("\n=== {model_label} F-M: SKIPPED (model not found) ===");
        return;
    }

    let mut session = hypr_onnx::load_model_from_path(&model_path).expect("load model");

    // Auto-detect tensor names
    let (inp, out) = NAME_SETS
        .iter()
        .find(|(inp, out)| probe_model(inp, out, &mut session))
        .map(|&(i, o)| (i, o))
        .unwrap_or_else(|| {
            eprintln!("  WARNING: could not find working tensor names for {model_label}");
            ("feats", "embs")
        });
    if model_label.contains("cnceleb") {
        println!("  {model_label}: using tensor names '{inp}'/'{out}'");
    }

    let mut hy = hypr_embedding::EmbeddingExtractor::new().unwrap();

    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data");
    let (female, _) = load_audio_f32(&base.join("female_welcome_1.mp3"), SR);
    let (male_1, _) = load_audio_f32(&base.join("male_welcome_1.mp3"), SR);
    let silence = vec![0.0f32; SR as usize];
    let combined: Vec<f32> = [female, silence, male_1].concat();
    let audio_i16: Vec<i16> = combined
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let mut segmenter = Segmenter::new(SR).unwrap();
    let segments = segmenter.process(&audio_i16, SR).unwrap();

    let mut ws_embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(segments.len());
    for seg in &segments {
        ws_embs.push(embed_ws_seg(seg, &mut session, inp, out));
    }

    let hy_embs: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            hy.compute_optional(&f32_s).ok().flatten()
        })
        .collect();

    print_wespeaker_comparison(model_label, "F-M concat", &ws_embs, &hy_embs, &segments);
}

fn run_wespeaker_korean_test(model_filename: &str, model_label: &str, _voxceleb_names: bool) {
    const SR: u32 = 16000;
    let model_path = std::path::Path::new(MODELS_DIR).join(model_filename);
    if !model_path.exists() {
        println!("\n=== {model_label} Korean: SKIPPED (model not found) ===");
        return;
    }

    let mut session = hypr_onnx::load_model_from_path(&model_path).expect("load model");

    let (inp, out) = NAME_SETS
        .iter()
        .find(|(inp, out)| probe_model(inp, out, &mut session))
        .map(|&(i, o)| (i, o))
        .unwrap_or_else(|| {
            eprintln!("  WARNING: could not find working tensor names for {model_label}");
            ("feats", "embs")
        });

    let mut hy = hypr_embedding::EmbeddingExtractor::new().unwrap();

    let audio_i16_orig = wav_bytes_to_i16_s16le(hypr_data::korean_1::AUDIO);
    let audio_f32: Vec<f32> = audio_i16_orig.iter().map(|&s| s as f32 / 32768.0).collect();
    let max_s = (SR * 30) as usize;
    let clip: Vec<f32> = if audio_f32.len() > max_s {
        audio_f32[..max_s].to_vec()
    } else {
        audio_f32
    };
    let clip_i16: Vec<i16> = clip
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let mut segmenter = Segmenter::new(SR).unwrap();
    let segments = segmenter.process(&clip_i16, SR).unwrap();

    let mut ws_embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(segments.len());
    for seg in &segments {
        ws_embs.push(embed_ws_seg(seg, &mut session, inp, out));
    }

    let hy_embs: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            hy.compute_optional(&f32_s).ok().flatten()
        })
        .collect();

    print_wespeaker_comparison(model_label, "korean_1 (30s)", &ws_embs, &hy_embs, &segments);
}

fn print_wespeaker_comparison(
    model_name: &str,
    label: &str,
    ws_embs: &[Option<Vec<f32>>],
    hy_embs: &[Option<Vec<f32>>],
    segments: &[Segment],
) {
    let ws_valid: Vec<(usize, &[f32])> = ws_embs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
        .collect();
    let hy_valid: Vec<(usize, &[f32])> = hy_embs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
        .collect();

    println!("\n=== Model Comparison: {model_name} ===");
    println!("{label}: {} segments", segments.len());
    println!(
        "{model_name} valid: {}/{} (dim={})",
        ws_valid.len(),
        segments.len(),
        ws_valid.first().map(|(_, e)| e.len()).unwrap_or(0)
    );
    println!(
        "hypr-embedding valid: {}/{} (dim={})",
        hy_valid.len(),
        segments.len(),
        hy_valid.first().map(|(_, e)| e.len()).unwrap_or(0)
    );

    if ws_valid.len() < 2 || hy_valid.len() < 2 {
        println!("  too few valid embeddings, skipping");
        return;
    }

    let ws_dist = {
        let n = ws_valid.len();
        let mut d = vec![0.0f32; n * n];
        for i in 0..n {
            for j in 0..n {
                d[i * n + j] = cosine_distance(ws_valid[i].1, ws_valid[j].1);
            }
        }
        d
    };
    let hy_dist = {
        let n = hy_valid.len();
        let mut d = vec![0.0f32; n * n];
        for i in 0..n {
            for j in 0..n {
                d[i * n + j] = cosine_distance(hy_valid[i].1, hy_valid[j].1);
            }
        }
        d
    };
    let ws_n = ws_valid.len();
    let hy_n = hy_valid.len();

    println!("\nDistance matrices ({model_name} left, hypr right):");
    for i in 0..ws_n.min(hy_n).min(6) {
        print!(" s{:<3} |", ws_valid[i].0);
        for j in 0..ws_n.min(6) {
            print!(" {:.4}", ws_dist[i * ws_n + j]);
        }
        let hi = hy_valid
            .iter()
            .position(|&(idx, _)| idx == ws_valid[i].0)
            .unwrap_or(i.min(hy_n - 1));
        print!("  ||  s{:<3} |", hy_valid[hi].0);
        for j in 0..hy_n.min(6) {
            let hj = hy_valid
                .iter()
                .position(|&(idx, _)| idx == ws_valid[j].0)
                .unwrap_or(j.min(hy_n - 1));
            print!(" {:.4}", hy_dist[hi * hy_n + hj]);
        }
        println!();
    }

    println!("\nThreshold sweep:");
    for &t in &[0.02, 0.03, 0.05, 0.08, 0.12, 0.20, 0.35, 0.45] {
        let wc = agglomerative_cluster(&ws_dist, ws_n, t)
            .iter()
            .max()
            .unwrap_or(&0)
            + 1;
        let hc = agglomerative_cluster(&hy_dist, hy_n, t)
            .iter()
            .max()
            .unwrap_or(&0)
            + 1;
        println!("  t={t:.2}: {model_name}={wc:2}  hypr={hc:2}");
    }

    let ws_same_avg = {
        let mut v = Vec::new();
        for i in 0..ws_n {
            for j in i + 1..ws_n {
                let d = ws_dist[i * ws_n + j];
                if d < 0.02 {
                    v.push(d);
                }
            }
        }
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f32>() / v.len() as f32
        }
    };
    let ws_diff_avg = {
        let mut v = Vec::new();
        for i in 0..ws_n {
            for j in i + 1..ws_n {
                let d = ws_dist[i * ws_n + j];
                if d >= 0.02 {
                    v.push(d);
                }
            }
        }
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f32>() / v.len() as f32
        }
    };
    let hy_same_avg = {
        let mut v = Vec::new();
        for i in 0..hy_n {
            for j in i + 1..hy_n {
                let d = hy_dist[i * hy_n + j];
                if d < 0.02 {
                    v.push(d);
                }
            }
        }
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f32>() / v.len() as f32
        }
    };
    let hy_diff_avg = {
        let mut v = Vec::new();
        for i in 0..hy_n {
            for j in i + 1..hy_n {
                let d = hy_dist[i * hy_n + j];
                if d >= 0.02 {
                    v.push(d);
                }
            }
        }
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f32>() / v.len() as f32
        }
    };
    println!("\nStats (auto threshold 0.02 for same/diff split):");
    println!(
        "  {model_name}: same_avg={ws_same_avg:.4} diff_avg={ws_diff_avg:.4} ratio={:.1}x",
        ws_diff_avg / ws_same_avg.max(0.001)
    );
    println!(
        "  hypr:      same_avg={hy_same_avg:.4} diff_avg={hy_diff_avg:.4} ratio={:.1}x",
        hy_diff_avg / hy_same_avg.max(0.001)
    );
}

#[test]
fn test_wespeaker_voxceleb_fm() {
    run_wespeaker_fm_test(
        "wespeaker-voxceleb-resnet34-LM.onnx",
        "wespeaker-voxceleb",
        true,
    );
}
#[test]
fn test_wespeaker_voxceleb_korean() {
    run_wespeaker_korean_test(
        "wespeaker-voxceleb-resnet34-LM.onnx",
        "wespeaker-voxceleb",
        true,
    );
}
#[test]
fn test_wespeaker_cnceleb_fm() {
    run_wespeaker_fm_test(
        "wespeaker_zh_cnceleb_resnet34.onnx",
        "wespeaker-cnceleb",
        false,
    );
}
#[test]
fn test_wespeaker_cnceleb_korean() {
    run_wespeaker_korean_test(
        "wespeaker_zh_cnceleb_resnet34.onnx",
        "wespeaker-cnceleb",
        false,
    );
}

// ===========================================================================
// Model comparison — 3D-Speaker CAM++ (192-dim, 200k speakers, zh+en)
// Auto-detects tensor names via probe_model() against NAME_SETS.
// ===========================================================================

fn run_campplus_fm_test(model_filename: &str, model_label: &str) {
    const SR: u32 = 16000;
    let model_path = std::path::Path::new(MODELS_DIR).join(model_filename);
    if !model_path.exists() {
        println!("\n=== {model_label} F-M: SKIPPED (model not found) ===");
        return;
    }

    let mut session = hypr_onnx::load_model_from_path(&model_path).expect("load model");

    let (inp, out) = NAME_SETS
        .iter()
        .find(|(inp, out)| probe_model(inp, out, &mut session))
        .map(|&(i, o)| (i, o))
        .unwrap_or_else(|| {
            eprintln!("  WARNING: could not find working tensor names for {model_label}");
            ("feats", "embs")
        });
    println!("  {model_label}: using tensor names '{inp}'/'{out}'");

    let mut hy = hypr_embedding::EmbeddingExtractor::new().unwrap();

    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data");
    let (female, _) = load_audio_f32(&base.join("female_welcome_1.mp3"), SR);
    let (male_1, _) = load_audio_f32(&base.join("male_welcome_1.mp3"), SR);
    let silence = vec![0.0f32; SR as usize];
    let combined: Vec<f32> = [female, silence, male_1].concat();
    let audio_i16: Vec<i16> = combined
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let mut segmenter = Segmenter::new(SR).unwrap();
    let segments = segmenter.process(&audio_i16, SR).unwrap();

    let mut cp_embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(segments.len());
    for seg in &segments {
        cp_embs.push(embed_ws_seg(seg, &mut session, inp, out));
    }

    let hy_embs: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            hy.compute_optional(&f32_s).ok().flatten()
        })
        .collect();

    print_wespeaker_comparison(model_label, "F-M concat", &cp_embs, &hy_embs, &segments);
}

#[test]
fn test_campplus_fm() {
    run_campplus_fm_test("campplus-zh-en.onnx", "campplus-zh-en");
}

#[test]
fn test_campplus_korean() {
    const SR: u32 = 16000;
    let model_path = std::path::Path::new(MODELS_DIR).join("campplus-zh-en.onnx");
    if !model_path.exists() {
        println!("\n=== campplus-zh-en Korean: SKIPPED (model not found) ===");
        return;
    }

    let mut session = hypr_onnx::load_model_from_path(&model_path).expect("load model");

    let (inp, out) = NAME_SETS
        .iter()
        .find(|(inp, out)| probe_model(inp, out, &mut session))
        .map(|&(i, o)| (i, o))
        .unwrap_or_else(|| {
            eprintln!("  WARNING: could not find working tensor names for campplus");
            ("feats", "embs")
        });
    println!("  campplus-zh-en: using tensor names '{inp}'/'{out}'");

    let mut hy = hypr_embedding::EmbeddingExtractor::new().unwrap();

    let audio_i16_orig = wav_bytes_to_i16_s16le(hypr_data::korean_1::AUDIO);
    let audio_f32: Vec<f32> = audio_i16_orig.iter().map(|&s| s as f32 / 32768.0).collect();
    let max_s = (SR * 30) as usize;
    let clip: Vec<f32> = if audio_f32.len() > max_s {
        audio_f32[..max_s].to_vec()
    } else {
        audio_f32
    };
    let clip_i16: Vec<i16> = clip
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let mut segmenter = Segmenter::new(SR).unwrap();
    let segments = segmenter.process(&clip_i16, SR).unwrap();

    let mut cp_embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(segments.len());
    for seg in &segments {
        cp_embs.push(embed_ws_seg(seg, &mut session, inp, out));
    }

    let hy_embs: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            hy.compute_optional(&f32_s).ok().flatten()
        })
        .collect();

    print_wespeaker_comparison("campplus-zh-en", "korean_1 (30s)", &cp_embs, &hy_embs, &segments);
}

// ===========================================================================
// CAM++ 200k version (welcomyou, feats/embs, 192-dim)
// ===========================================================================

#[test]
fn test_campplus_200k_fm() {
    run_campplus_fm_test("campplus_cn_en_common_200k.onnx", "campplus-200k");
}

#[test]
fn test_campplus_200k_korean() {
    const SR: u32 = 16000;
    let model_path = std::path::Path::new(MODELS_DIR).join("campplus_cn_en_common_200k.onnx");
    if !model_path.exists() {
        println!("\n=== campplus-200k Korean: SKIPPED (model not found) ===");
        return;
    }

    let mut session = hypr_onnx::load_model_from_path(&model_path).expect("load model");

    let (inp, out) = NAME_SETS
        .iter()
        .find(|(inp, out)| probe_model(inp, out, &mut session))
        .map(|&(i, o)| (i, o))
        .unwrap_or_else(|| {
            eprintln!("  WARNING: could not find working tensor names for campplus-200k");
            ("feats", "embs")
        });
    println!("  campplus-200k: using tensor names '{inp}'/'{out}'");

    let mut hy = hypr_embedding::EmbeddingExtractor::new().unwrap();

    let audio_i16_orig = wav_bytes_to_i16_s16le(hypr_data::korean_1::AUDIO);
    let audio_f32: Vec<f32> = audio_i16_orig.iter().map(|&s| s as f32 / 32768.0).collect();
    let max_s = (SR * 30) as usize;
    let clip: Vec<f32> = if audio_f32.len() > max_s {
        audio_f32[..max_s].to_vec()
    } else {
        audio_f32
    };
    let clip_i16: Vec<i16> = clip
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let mut segmenter = Segmenter::new(SR).unwrap();
    let segments = segmenter.process(&clip_i16, SR).unwrap();

    let mut cp_embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(segments.len());
    for seg in &segments {
        cp_embs.push(embed_ws_seg(seg, &mut session, inp, out));
    }

    let hy_embs: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|seg| {
            let f32_s: Vec<f32> = seg.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            hy.compute_optional(&f32_s).ok().flatten()
        })
        .collect();

    print_wespeaker_comparison("campplus-200k", "korean_1 (30s)", &cp_embs, &hy_embs, &segments);
}

// ===========================================================================
// Wespeaker CN-Celeb ResNet34 LM (Large Margin fine-tuned, recommended)
// ===========================================================================

#[test]
fn test_wespeaker_cnceleb_lm_fm() {
    run_wespeaker_fm_test(
        "wespeaker_zh_cnceleb_resnet34_LM.onnx",
        "wespeaker-cnceleb-LM",
        false,
    );
}

#[test]
fn test_wespeaker_cnceleb_lm_korean() {
    run_wespeaker_korean_test(
        "wespeaker_zh_cnceleb_resnet34_LM.onnx",
        "wespeaker-cnceleb-LM",
        false,
    );
}

// ===========================================================================
// Chinese speaker diarization (SOND demo files from Alibaba OSS)
// spk1.wav ~ spk4.wav: 4 individual speakers, 15s each
// record.wav: multi-speaker mixed conversation, ~15s
// ===========================================================================

const SOND_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/sond");

fn load_wav_i16(path: &Path) -> (Vec<i16>, u32) {
    let file = std::fs::File::open(path).unwrap();
    let source = rodio::Decoder::try_from(file).unwrap();
    let sr: u32 = source.sample_rate().into();
    let f32s: Vec<f32> = source.collect();
    let i16s: Vec<i16> = f32s.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
    (i16s, sr)
}

fn embed_with_model(seg: &Segment, session: &mut Session, inp: &str, out: &str) -> Option<Vec<f32>> {
    if seg.samples.len() < 16000 {
        return None;
    }
    embed_ws_seg(seg, session, inp, out)
}

fn run_cn_speaker_test(
    model_path: &Path,
    model_label: &str,
    model_dim: usize,
) {
    let sond = Path::new(SOND_DIR);
    let spk_files = ["spk1.wav", "spk2.wav", "spk3.wav", "spk4.wav"];
    let mut spk_embs: Vec<(&str, Option<Vec<f32>>)> = Vec::new();

    let mut segmenter = Segmenter::new(16000).unwrap();
    let mut session = hypr_onnx::load_model_from_path(model_path).expect("load model");

    let (inp, out) = NAME_SETS
        .iter()
        .find(|(inp, out)| probe_model(inp, out, &mut session))
        .map(|&(i, o)| (i, o))
        .unwrap_or_else(|| {
            eprintln!("  WARNING: could not find tensor names for {model_label}");
            ("feats", "embs")
        });

    for &fname in &spk_files {
        let path = sond.join(fname);
        if !path.exists() { continue; }
        let (audio_i16, sr) = load_wav_i16(&path);
        let segments = segmenter.process(&audio_i16, sr).unwrap();
        let mut best_emb: Option<Vec<f32>> = None;
        for seg in &segments {
            if seg.samples.len() < 24000 { continue; }
            if let Some(emb) = embed_with_model(seg, &mut session, inp, out) {
                if emb.len() == model_dim {
                    best_emb = Some(emb);
                    break;
                }
            }
        }
        spk_embs.push((fname, best_emb));
    }

    println!("\n=== Chinese Speaker Discrimination: {model_label} (dim={model_dim}) ===");
    let valid: Vec<(&&str, &[f32])> = spk_embs.iter()
        .filter_map(|(n, e)| e.as_deref().map(|e| (n, e)))
        .collect();
    println!("valid speakers: {}/{}", valid.len(), spk_files.len());
    for &(name, emb) in &valid {
        let mean = emb.iter().sum::<f32>() / emb.len() as f32;
        println!("  {name}: dim={} mean={:.4}", emb.len(), mean);
    }

    if valid.len() < 2 { return; }

    let n = valid.len();
    let mut dist = vec![0.0f32; n * n];
    for i in 0..n {
        for j in i + 1..n {
            let d = cosine_distance(valid[i].1, valid[j].1);
            dist[i * n + j] = d;
            dist[j * n + i] = d;
        }
    }

    println!("\nCross-speaker distance matrix:"); // Print like a table later
    print!("          ");
    for i in 0..n { print!("  {:<8}", valid[i].0); }
    println!();
    for i in 0..n {
        print!("{:<8} ", valid[i].0);
        for j in 0..n {
            print!("  {:.4}", dist[i * n + j]);
        }
        println!();
    }

    let distinct = n * (n - 1) / 2;
    let avg_dist = {
        let mut s = 0.0f32;
        let mut c = 0usize;
        for i in 0..n { for j in i + 1..n { s += dist[i * n + j]; c += 1; } }
        s / c as f32
    };
    println!("\n{model_label}: {distinct} speaker pairs, avg cross={avg_dist:.4}");

    println!("\nClustering (threshold=0.35):");
    let cids = agglomerative_cluster(&dist, n, 0.35);
    let nc = cids.iter().max().unwrap_or(&0) + 1;
    for (i, &c) in cids.iter().enumerate() {
        println!("  {} → Speaker_{}", valid[i].0, c);
    }
    println!("  {nc} clusters (GT=4)");
}

#[test]
fn test_chinese_speaker_all_models() {
    let models: Vec<(&str, &str, usize)> = vec![
        ("campplus_cn_en_common_200k.onnx", "campplus-200k", 192),
        ("wespeaker-voxceleb-resnet34-LM.onnx", "wespeaker-voxceleb", 256),
        ("wespeaker_zh_cnceleb_resnet34.onnx", "wespeaker-cnceleb", 256),
        ("wespeaker_zh_cnceleb_resnet34_LM.onnx", "wespeaker-cnceleb-LM", 256),
    ];
    let base = Path::new(MODELS_DIR);

    for (fname, label, dim) in &models {
        let path = base.join(fname);
        if path.exists() {
            run_cn_speaker_test(&path, label, *dim);
        } else {
            println!("\n=== {label}: SKIPPED (not found) ===");
        }
    }
}

// ===========================================================================
// 中文多说话人混合音频 (record.wav, ~15s, 3-4 speakers)
// ===========================================================================

#[test]
fn test_chinese_record_diarization() {
    const SR: u32 = 16000;
    let path = Path::new(SOND_DIR).join("record.wav");
    if !path.exists() {
        println!("\n=== Chinese record diarization: SKIPPED ===");
        return;
    }

    let (audio_i16, sr) = load_wav_i16(&path);
    println!("\n=== Chinese Record Diarization ===");
    println!("duration: {:.2}s", audio_i16.len() as f64 / sr as f64);

    let mut segmenter = Segmenter::new(SR).unwrap();
    let segments = segmenter.process(&audio_i16, sr).unwrap();
    println!("segments found: {}", segments.len());

    // Test each model
    let models: Vec<(&str, &str, usize)> = vec![
        ("campplus_cn_en_common_200k.onnx", "campplus-200k", 192),
        ("wespeaker-voxceleb-resnet34-LM.onnx", "wespeaker-voxceleb", 256),
        ("wespeaker_zh_cnceleb_resnet34_LM.onnx", "wespeaker-cnceleb-LM", 256),
    ];

    let base = Path::new(MODELS_DIR);
    for (fname, label, _dim) in &models {
        let model_path = base.join(fname);
        if !model_path.exists() { continue; }

        let mut session = hypr_onnx::load_model_from_path(&model_path).expect("load model");
        let (inp, out) = NAME_SETS
            .iter()
            .find(|(inp, out)| probe_model(inp, out, &mut session))
            .map(|&(i, o)| (i, o))
            .unwrap_or(("feats", "embs"));

        let t0 = std::time::Instant::now();
        let mut embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(segments.len());
        for seg in &segments {
            embs.push(if seg.samples.len() >= 24000 {
                embed_with_model(seg, &mut session, inp, out)
            } else {
                None
            });
        }
        let embed_time = t0.elapsed();

        let valid: Vec<(usize, &[f32])> = embs.iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
            .collect();
        println!("\n  {label}: {}/{} valid embs ({:.3}s)",
            valid.len(), segments.len(), embed_time.as_secs_f64());

        if valid.len() < 3 { continue; }

        let n = valid.len();
        let mut dist = vec![0.0f32; n * n];
        for i in 0..n { for j in i + 1..n {
            let d = cosine_distance(valid[i].1, valid[j].1);
            dist[i * n + j] = d; dist[j * n + i] = d;
        }}

        println!("    Threshold sweep:");
        for &t in &[0.02, 0.08, 0.20, 0.35, 0.45, 0.55] {
            let c = agglomerative_cluster(&dist, n, t);
            let nc = c.iter().max().unwrap_or(&0) + 1;
            print!(" t={t:.2}→{nc}");
        }
        println!();
    }
}

// ===========================================================================
// 3-minute benchmark: full diarization pipeline timing per model
// ===========================================================================

#[test]
fn test_benchmark_3min_chinese() {
    const SR: u32 = 16000;
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/data/chinese_3min.wav");
    if !path.exists() {
        println!("\n=== 3min benchmark: SKIPPED (no chinese_3min.wav) ===");
        return;
    }

    let (audio_i16, sr) = load_wav_i16(&path);
    let duration_s = audio_i16.len() as f64 / sr as f64;
    println!("\n=== 3-Minute Benchmark ===");
    println!("audio duration: {duration_s:.1}s ({:.1}min)", duration_s / 60.0);
    println!("samples: {}", audio_i16.len());

    // segmentation timing (shared)
    let t_seg = std::time::Instant::now();
    let mut segmenter = Segmenter::new(SR).unwrap();
    let segments = segmenter.process(&audio_i16, sr).unwrap();
    let seg_time = t_seg.elapsed();
    println!("segmentation: {:.3}s ({} segments)", seg_time.as_secs_f64(), segments.len());

    let models: Vec<(&str, &str, usize)> = vec![
        ("campplus_cn_en_common_200k.onnx", "campplus-200k", 192),
        ("wespeaker-voxceleb-resnet34-LM.onnx", "wespeaker-voxceleb", 256),
        ("wespeaker_zh_cnceleb_resnet34.onnx", "wespeaker-cnceleb", 256),
        ("wespeaker_zh_cnceleb_resnet34_LM.onnx", "wespeaker-cnceleb-LM", 256),
    ];

    let base = Path::new(MODELS_DIR);
    println!("\n{:<20} {:>10} {:>10} {:>10} {:>12} {:>12} {:>15}",
        "Model", "Dim", "Segs", "Emb/s", "Embed(s)", "Cluster(s)", "Total(s)");
    println!("{}", "-".repeat(85));

    for (fname, label, dim) in &models {
        let model_path = base.join(fname);
        if !model_path.exists() { continue; }

        let mut session = hypr_onnx::load_model_from_path(&model_path).expect("load model");
        let (inp, out) = NAME_SETS
            .iter()
            .find(|(inp, out)| probe_model(inp, out, &mut session))
            .map(|&(i, o)| (i, o))
            .unwrap_or(("feats", "embs"));

        let t0 = std::time::Instant::now();
        let mut embs: Vec<Option<Vec<f32>>> = Vec::with_capacity(segments.len());
        for seg in &segments {
            embs.push(if seg.samples.len() >= 16000 {
                embed_with_model(seg, &mut session, inp, out)
            } else {
                None
            });
        }
        let embed_time = t0.elapsed();

        let valid: Vec<(usize, &[f32])> = embs.iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_deref().map(|e| (i, e)))
            .collect();

        let t1 = std::time::Instant::now();
        if valid.len() >= 2 {
            let n = valid.len();
            let mut dist = vec![0.0f32; n * n];
            for i in 0..n { for j in i + 1..n {
                let d = cosine_distance(valid[i].1, valid[j].1);
                dist[i * n + j] = d; dist[j * n + i] = d;
            }}
            let _cids = agglomerative_cluster(&dist, n, 0.35);
        }
        let cluster_time = t1.elapsed();

        let total = seg_time + embed_time + cluster_time;
        let emb_per_s = if embed_time.as_secs_f64() > 0.0 {
            segments.len() as f64 / embed_time.as_secs_f64()
        } else { 0.0 };

        println!("{label:<20} {dim:>4}dim {n_segs:>4} {emb_per_s:>8.1}/s {embed_time:>8.3}s {cluster_time:>8.3}s {total:>9.3}s",
            n_segs = segments.len(),
            emb_per_s = emb_per_s,
            embed_time = embed_time.as_secs_f64(),
            cluster_time = cluster_time.as_secs_f64(),
            total = total.as_secs_f64(),
        );
    }
}
