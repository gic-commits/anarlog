// Agglomerative clustering with average linkage

/// 对 n 个 embedding 做 agglomerative clustering
/// `dist` 是 n×n 的 cosine distance matrix (column-major)
/// `threshold`: 合并阈值，低于此值的 cluster 会合并
pub fn agglomerative_cluster(dist: &[f32], n: usize, threshold: f32) -> Vec<usize> {
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

/// Cosine distance (1 - cosine similarity)
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denom = na * nb;
    if denom == 0.0 {
        return 1.0;
    }
    1.0 - (dot / denom).clamp(-1.0, 1.0)
}

/// 从 embeddings 构建 distance matrix
pub fn build_distance_matrix(embeddings: &[Vec<f32>]) -> Vec<f32> {
    let n = embeddings.len();
    let mut dist = vec![0.0f32; n * n];
    for i in 0..n {
        for j in i + 1..n {
            let d = cosine_distance(&embeddings[i], &embeddings[j]);
            dist[i * n + j] = d;
            dist[j * n + i] = d;
        }
    }
    dist
}

/// 聚类 embeddings，返回 cluster assignments
pub fn cluster_embeddings(embeddings: &[Vec<f32>], threshold: f32) -> Vec<usize> {
    if embeddings.is_empty() {
        return vec![];
    }
    if embeddings.len() == 1 {
        return vec![0];
    }
    let dist = build_distance_matrix(embeddings);
    agglomerative_cluster(&dist, embeddings.len(), threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_segment() {
        let c = cluster_embeddings(&[vec![1.0, 0.0]], 0.35);
        assert_eq!(c, vec![0]);
    }

    #[test]
    fn test_two_close_segments() {
        let c = cluster_embeddings(
            &[vec![1.0, 0.0], vec![0.99, 0.01]],
            0.35,
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], c[1]); // same cluster
    }

    #[test]
    fn test_two_far_segments() {
        let c = cluster_embeddings(
            &[vec![1.0, 0.0], vec![-1.0, 0.0]],
            0.35,
        );
        assert_eq!(c.len(), 2);
        assert_ne!(c[0], c[1]); // different clusters
    }

    #[test]
    fn test_cosine_distance_same() {
        let d = cosine_distance(&[1.0, 0.0], &[1.0, 0.0]);
        assert!(d.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_distance_orthogonal() {
        let d = cosine_distance(&[1.0, 0.0], &[0.0, 1.0]);
        assert!((d - 1.0).abs() < 1e-6);
    }
}
