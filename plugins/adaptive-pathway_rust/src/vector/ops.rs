//! Pure numeric ops over `f32`/`f64` vectors. Stands in for the handful of
//! numpy operations the Python original actually used (L2 norms, dot
//! products, argsort/argpartition).

/// Index of the argmax of `scores` (first max on ties). Empty -> None.
pub fn argmax(scores: &[f64]) -> Option<usize> {
    if scores.is_empty() {
        return None;
    }
    let mut best = 0usize;
    let mut best_val = scores[0];
    for (i, &v) in scores.iter().enumerate().skip(1) {
        if v > best_val {
            best = i;
            best_val = v;
        }
    }
    Some(best)
}

/// Indices sorted by score descending (stable by original index for ties).
/// Equal to numpy `argsort(-scores)` for the full range.
pub fn argsort_desc(scores: &[f64]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..scores.len()).collect();
    idx.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));
    idx
}

/// Top-k indices by score descending, ties broken by original index so the
/// result is a deterministic function of the input (mirrors numpy
/// argpartition-then-argsort semantics but total).
pub fn topk_desc(scores: &[f64], k: usize) -> Vec<usize> {
    if k == 0 || scores.is_empty() {
        return vec![];
    }
    let mut idx: Vec<usize> = (0..scores.len()).collect();
    idx.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));
    idx.truncate(k.min(scores.len()));
    idx
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum()
}

pub fn norm(a: &[f32]) -> f32 {
    a.iter().map(|&x| x * x).sum::<f32>().sqrt()
}

/// Cosine similarity between two vectors. Zero-vector (or either zero-norm)
/// returns 0.0, matching the Python guard that skips near-zero queries.
/// Mismatched dimensions also return 0.0 (treated as unrelated) rather than
/// silently comparing whatever prefix `dot`'s `zip` happens to line up --
/// `a`/`b` typically come from independently-stored embeddings (a query
/// embedding against a persisted belief embedding), and a dimension
/// mismatch (a corrupted BLOB, or a belief embedded under a since-changed
/// `embedding_dim`/model) would otherwise silently divide a partial dot
/// product by the *full* norms of both vectors -- a number that looks like
/// a valid similarity score but measures nothing real.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let na = norm(a);
    let nb = norm(b);
    if na < 1e-12 || nb < 1e-12 {
        return 0.0;
    }
    (dot(a, b) as f64) / ((na as f64) * (nb as f64))
}

/// Normalize in place; a zero vector stays zero.
pub fn normalize_in_place(a: &mut [f32]) {
    let n = norm(a);
    if n < 1e-12 {
        a.fill(0.0);
        return;
    }
    let inv = 1.0 / n;
    for x in a.iter_mut() {
        *x *= inv;
    }
}

/// Momentum-extrapolate `current` forward along its trajectory from
/// `previous`: `current + momentum·(current − previous)`, renormalized.
/// The cheap, non-ML stand-in for a trained predictive (JEPA-style) recall
/// model — see `config::TrajectoryConfig`'s doc comment. Falls back to
/// returning `current` unchanged (well-formed, since it's already whatever
/// shape the caller gave it) on a length mismatch between `current` and
/// `previous` — e.g. a session whose embedding model changed mid-flight —
/// rather than computing a meaningless cross-space delta.
pub fn extrapolate(current: &[f32], previous: &[f32], momentum: f64) -> Vec<f32> {
    if current.len() != previous.len() {
        return current.to_vec();
    }
    let momentum = momentum as f32;
    let mut predicted: Vec<f32> = current
        .iter()
        .zip(previous.iter())
        .map(|(&c, &p)| c + momentum * (c - p))
        .collect();
    normalize_in_place(&mut predicted);
    predicted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argmax_picks_highest() {
        assert_eq!(argmax(&[0.5, 0.9, 0.3]), Some(1));
        assert_eq!(argmax(&[]), None);
    }

    #[test]
    fn topk_desc_orders_and_ties_are_stable() {
        let scores = [0.5, 0.9, 0.9, 0.3];
        assert_eq!(topk_desc(&scores, 3), vec![1, 2, 0]);
        assert_eq!(topk_desc(&scores, 10), vec![1, 2, 0, 3]);
        assert_eq!(topk_desc(&scores, 0), Vec::<usize>::new());
    }

    #[test]
    fn cosine_handles_zero() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]).abs() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn extrapolate_continues_the_trajectory_past_current() {
        // Moving from [1,0] to [0,1] (a 90-degree turn); extrapolating with
        // momentum should land further along that same turn than `current`
        // alone, i.e. cosine(current, predicted) < cosine(current, current)
        // and the predicted direction leans away from `previous`.
        let previous = vec![1.0_f32, 0.0];
        let current = vec![0.0_f32, 1.0];
        let predicted = extrapolate(&current, &previous, 0.5);
        // predicted = current + 0.5*(current-previous) = [-0.5, 1.5], normalized
        let expected_dir = {
            let mut v = vec![-0.5_f32, 1.5];
            normalize_in_place(&mut v);
            v
        };
        for (p, e) in predicted.iter().zip(expected_dir.iter()) {
            assert!((p - e).abs() < 1e-6);
        }
    }

    #[test]
    fn extrapolate_zero_momentum_is_current_normalized() {
        let previous = vec![1.0_f32, 0.0];
        let current = vec![0.6_f32, 0.8]; // already unit length
        let predicted = extrapolate(&current, &previous, 0.0);
        for (p, c) in predicted.iter().zip(current.iter()) {
            assert!((p - c).abs() < 1e-6);
        }
    }

    #[test]
    fn extrapolate_mismatched_dims_falls_back_to_current() {
        let previous = vec![1.0_f32, 0.0, 0.0];
        let current = vec![0.0_f32, 1.0];
        assert_eq!(extrapolate(&current, &previous, 0.5), current);
    }

    #[test]
    fn cosine_mismatched_dimensions_is_unrelated_not_a_partial_match() {
        // Without the length guard, `dot`'s zip would silently compare only
        // the first 2 components against the full 3-component norm of `b`,
        // producing a nonzero "similarity" for two vectors that don't even
        // live in the same space.
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
