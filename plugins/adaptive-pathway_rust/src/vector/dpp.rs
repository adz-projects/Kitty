//! Greedy DPP (determinantal point process) max-marginal sampling, ported
//! from `decision/diversity.py`. Kernel = W·S·W with S the cosine-similarity
//! matrix between (normalized) embeddings and W = diag(scores·diversity_w).
//! The greedy MAP iterate performs a rank-1 downdate against the selected
//! item, which is what lets two near-equal-strength similar items both
//! survive in the top-k (the first suppresses the duplicate only when it is
//! *much* stronger).

use super::ops;

/// Build the DPP kernel M = W·S·W. Returns an n×n row-major matrix.
pub fn build_dpp_kernel(
    embeddings: &[Vec<f32>],
    scores: &[f64],
    diversity_weight: f64,
) -> Vec<Vec<f64>> {
    let n = scores.len();
    if n == 0 {
        return vec![];
    }
    // Normalize embeddings.
    let mut normed: Vec<Vec<f32>> = embeddings.to_vec();
    for e in normed.iter_mut() {
        let nrm = ops::norm(e);
        let nrm = if nrm < 1e-12 { 1.0 } else { nrm };
        for x in e.iter_mut() {
            *x /= nrm;
        }
    }
    // S = cosine similarity between normalized embeddings
    let s = similarity_matrix(&normed);
    // M = W·S·W, W = diag(scores * diversity_weight)
    let mut kernel = vec![vec![0.0; n]; n];
    for i in 0..n {
        let wi = scores[i] * diversity_weight;
        for j in 0..n {
            let wj = scores[j] * diversity_weight;
            kernel[i][j] = wi * s[i][j] * wj;
        }
    }
    kernel
}

fn similarity_matrix(normed: &[Vec<f32>]) -> Vec<Vec<f64>> {
    let n = normed.len();
    let mut s = vec![vec![0.0; n]; n];
    for i in 0..n {
        s[i][i] = 1.0;
        for j in (i + 1)..n {
            let c = ops::dot(&normed[i], &normed[j]) as f64;
            s[i][j] = c;
            s[j][i] = c;
        }
    }
    s
}

/// Greedy MAP sample of `k` items from an n×n kernel. Returns selected
/// original indices. Ported verbatim from `dpp_sample`.
pub fn dpp_sample(kernel: &[Vec<f64>], k: usize, epsilon: f64) -> Vec<usize> {
    let n = kernel.len();
    if n == 0 || k == 0 {
        return vec![];
    }
    let k = k.min(n);
    let mut selected: Vec<usize> = Vec::with_capacity(k);
    let mut remaining: Vec<usize> = (0..n).collect();
    let mut l = kernel.to_vec();

    for _ in 0..k {
        // Diagonal of L restricted to `remaining`.
        let diag: Vec<f64> = remaining.iter().map(|&i| l[i][i]).collect();
        if diag.iter().all(|&d| d <= 0.0) {
            // All-nonpositive-diagonal fallback: pick the remaining item with
            // the largest absolute coupling to the rest.
            let mut best = remaining[0];
            let mut best_val = f64::NEG_INFINITY;
            for &i in &remaining {
                let val: f64 = remaining.iter().map(|&j| l[i][j].abs()).sum();
                if val > best_val {
                    best_val = val;
                    best = i;
                }
            }
            selected.push(best);
            remaining.retain(|&x| x != best);
            continue;
        }
        let best_local = remaining[ops::argmax(&diag).unwrap()];
        selected.push(best_local);
        remaining.retain(|&x| x != best_local);
        if remaining.is_empty() {
            break;
        }
        // L_sel = L[best_local, remaining]; the outer product L_sel·L_sel^T /
        // L_ss is subtracted from the restricted submatrix L[remaining,
        // remaining] (Python `L_sub - np.outer(L_sel, L_sel)/L_ss`). L is
        // symmetric, so L[best, j] == L[j, best].
        let l_sel: Vec<f64> = remaining.iter().map(|&idx| l[best_local][idx]).collect();
        let l_ss = l[best_local][best_local].max(epsilon);
        for (ai, &ri) in remaining.iter().enumerate() {
            for (bi, &rj) in remaining.iter().enumerate() {
                l[ri][rj] -= l_sel[ai] * l_sel[bi] / l_ss;
            }
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rnd_vec(dim: usize, seed: u64) -> Vec<f32> {
        // deterministic pseudo-random
        let mut x = seed;
        (0..dim)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 33) as f64 / u32::MAX as f64 - 0.5) as f32
            })
            .collect()
    }

    #[test]
    fn empty_kernel_empty() {
        let k = build_dpp_kernel(&[], &[], 1.0);
        assert_eq!(k, Vec::<Vec<f64>>::new());
        assert!(dpp_sample(&k, 3, 1e-7).is_empty());
    }

    #[test]
    fn shape_and_symmetric() {
        let embs: Vec<Vec<f32>> = (0..5).map(|i| rnd_vec(384, i)).collect();
        let scores = vec![0.5, 0.7, 0.3, 0.9, 0.6];
        let k = build_dpp_kernel(&embs, &scores, 1.0);
        assert_eq!(k.len(), 5);
        assert_eq!(k[0].len(), 5);
        for (i, row) in k.iter().enumerate() {
            for (j, &v) in row.iter().enumerate() {
                assert!((v - k[j][i]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn sample_unique_and_in_bounds() {
        let embs: Vec<Vec<f32>> = (0..10).map(|i| rnd_vec(384, i)).collect();
        let scores: Vec<f64> = (0..10).map(|i| 0.5 + 0.05 * i as f64).collect();
        let k = build_dpp_kernel(&embs, &scores, 1.0);
        let sel = dpp_sample(&k, 5, 1e-7);
        assert_eq!(sel.len(), 5);
        let mut uniq = sel.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 5);
        assert!(sel.iter().all(|&i| i < 10));
    }

    #[test]
    fn k_larger_than_n() {
        let embs: Vec<Vec<f32>> = (0..3).map(|i| rnd_vec(384, i)).collect();
        let scores = vec![0.5, 0.7, 0.9];
        let k = build_dpp_kernel(&embs, &scores, 1.0);
        assert_eq!(dpp_sample(&k, 10, 1e-7).len(), 3);
    }

    #[test]
    fn k_zero() {
        let embs: Vec<Vec<f32>> = (0..5).map(|i| rnd_vec(384, i)).collect();
        let scores = vec![0.5; 5];
        let k = build_dpp_kernel(&embs, &scores, 1.0);
        assert!(dpp_sample(&k, 0, 1e-7).is_empty());
    }

    #[test]
    fn deterministic() {
        let embs: Vec<Vec<f32>> = (0..5).map(|i| rnd_vec(384, i)).collect();
        let scores = vec![0.6, 0.7, 0.3, 0.8, 0.5];
        let k = build_dpp_kernel(&embs, &scores, 1.0);
        let a = dpp_sample(&k, 3, 1e-7);
        let b = dpp_sample(&k, 3, 1e-7);
        assert_eq!(a, b);
    }
}
