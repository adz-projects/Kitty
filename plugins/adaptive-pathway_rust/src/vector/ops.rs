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
    fn cosine_mismatched_dimensions_is_unrelated_not_a_partial_match() {
        // Without the length guard, `dot`'s zip would silently compare only
        // the first 2 components against the full 3-component norm of `b`,
        // producing a nonzero "similarity" for two vectors that don't even
        // live in the same space.
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
