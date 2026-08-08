//! Spreading activation: an on-the-fly cosine graph diffusion over the
//! recall candidate set, run between blended-score computation and DPP
//! selection (`recall::select_beliefs_relevant`). No persisted hyperedges —
//! the graph is built fresh each call from whatever candidates survived
//! suppression/relevance filtering, using the same cosine primitive
//! `vector/dpp.rs` already relies on (`ops::dot` over pre-normalized
//! vectors, i.e. cosine similarity).
//!
//! Anchors are the top-scoring candidates (by blended relevance score);
//! energy spreads outward across edges whose cosine similarity clears
//! `edge_threshold`, decaying by `gamma` per hop, for `hops` iterations.
//! Anchors do not self-seed their own activation — activation only ever
//! represents energy *received* from another candidate, so a candidate
//! that happens to be an anchor gets no free boost from being one; it can
//! still receive activation from a different anchor, or (with `hops >= 2`)
//! energy reflected back through a shared neighbor. The result folds into
//! each candidate's DPP input score as a multiplicative boost — DPP still
//! performs the final diversification pass, this only reweights what's
//! competing for a slot, favoring "ambient context" (a belief connected to
//! something clearly relevant) over an isolated belief that shares no edges
//! with anything else in play.

use crate::config::DiffusionConfig;
use crate::vector::ops;

/// Number of anchor seeds: `min(this, candidates.len())`. Kept small and
/// fixed rather than config-tunable — past a handful of anchors, diffusion
/// stops meaningfully differing from "boost everything a little."
const ANCHOR_COUNT: usize = 3;

/// Diffuse activation energy over the candidate set's graph. `normed_embeddings`
/// must already be unit length (same convention as
/// `vector::dpp::build_dpp_kernel_from_normalized` — a zero vector stays
/// zero). `seed_scores` seeds both anchor selection and each anchor's
/// starting energy (typically the same blended scores DPP will use).
/// Returns one activation value per candidate, aligned by index, `0.0` for
/// anything diffusion never reached (including every candidate when
/// `cfg.enabled` is `false`).
///
/// The graph has two edge types, traversed in the same hop loop:
///
/// - **Cosine edges**, weighted by similarity and gated on `edge_threshold` —
///   "this belief is about the same thing".
/// - **Co-occurrence edges** (`cooccurrence`, an adjacency list indexed the
///   same way as `normed_embeddings`), unweighted and *not* threshold-gated —
///   "these were observed together in one extraction batch". They deliberately
///   bypass `edge_threshold`, which is a cosine concept: co-occurring facts
///   are usually semantically distant from each other, which is precisely why
///   the cosine graph cannot see the relation and why recording it separately
///   is worth a migration. Their energy is scaled by
///   `cfg.cooccurrence_weight` instead.
///
/// An empty `cooccurrence` reduces this to pure cosine diffusion, byte-identical
/// to the pre-co-occurrence behaviour.
pub fn diffuse_activation(
    normed_embeddings: &[Vec<f32>],
    seed_scores: &[f64],
    cooccurrence: &[Vec<usize>],
    cfg: &DiffusionConfig,
) -> Vec<f64> {
    let n = normed_embeddings.len();
    let mut activation = vec![0.0_f64; n];
    if !cfg.enabled || n == 0 || cfg.hops == 0 {
        return activation;
    }

    // Anchors: top ANCHOR_COUNT candidates by seed score, ties broken by
    // index so the result is a deterministic function of input order.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        seed_scores[b]
            .partial_cmp(&seed_scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(&b))
    });
    let anchor_count = ANCHOR_COUNT.min(n);
    let mut frontier: Vec<(usize, f64)> = order[..anchor_count]
        .iter()
        .map(|&i| (i, seed_scores[i].max(0.0)))
        .collect();

    // O(hops * frontier_size * n) edge evaluations — bounded, since both
    // the anchor count and hop count are small fixed constants regardless
    // of how large the candidate set grows.
    for _ in 0..cfg.hops {
        let mut next: Vec<(usize, f64)> = Vec::new();
        for &(i, energy) in &frontier {
            if energy <= 0.0 {
                continue;
            }
            for (j, nj) in normed_embeddings.iter().enumerate() {
                if j == i {
                    continue;
                }
                let sim = ops::dot(&normed_embeddings[i], nj) as f64;
                if sim < cfg.edge_threshold {
                    continue;
                }
                let carried = energy * cfg.gamma * sim;
                if carried > activation[j] {
                    activation[j] = carried;
                    next.push((j, carried));
                }
            }
            // Co-occurrence edges: same decay, flat weight, no threshold.
            // Bounds-checked rather than assumed aligned so a caller that
            // builds a short adjacency (or none) degrades to cosine-only
            // diffusion instead of panicking in the per-turn recall path.
            if let Some(siblings) = cooccurrence.get(i) {
                for &j in siblings {
                    if j == i || j >= n {
                        continue;
                    }
                    let carried = energy * cfg.gamma * cfg.cooccurrence_weight;
                    if carried > activation[j] {
                        activation[j] = carried;
                        next.push((j, carried));
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    activation
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> DiffusionConfig {
        DiffusionConfig {
            enabled,
            gamma: 0.5,
            hops: 1,
            edge_threshold: 0.55,
            boost_weight: 0.3,
            cooccurrence_weight: 0.25,
        }
    }

    #[test]
    fn disabled_yields_all_zero() {
        let embs = vec![vec![1.0_f32, 0.0], vec![0.9_f32, 0.1]];
        let scores = vec![0.5, 0.1];
        let a = diffuse_activation(&embs, &scores, &[], &cfg(false));
        assert_eq!(a, vec![0.0, 0.0]);
    }

    #[test]
    fn empty_is_empty() {
        let a = diffuse_activation(&[], &[], &[], &cfg(true));
        assert!(a.is_empty());
    }

    #[test]
    fn energy_only_crosses_edges_above_threshold() {
        // a-b are near-identical (cosine ~0.9487, above 0.55); a-c are
        // orthogonal (cosine 0.0, below threshold). Diffusing from anchor a
        // must reach b but never c.
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.9_f32, 0.3, 0.0];
        let mut b_n = b.clone();
        ops::normalize_in_place(&mut b_n);
        let c = vec![0.0_f32, 0.0, 1.0];
        let embs = vec![a.clone(), b_n, c];
        // a strong (anchor), b and c both weak so they aren't anchors
        // themselves -- only ANCHOR_COUNT=3 seeds here, so with exactly 3
        // candidates all three ARE anchors; that's fine, b and c only
        // differ in whether they receive energy from a.
        let scores = vec![0.6, 0.05, 0.05];
        let act = diffuse_activation(&embs, &scores, &[], &cfg(true));
        assert!(act[1] > 0.0, "b must receive diffused energy from a");
        assert_eq!(act[2], 0.0, "c is orthogonal to everything, must receive none");
    }

    #[test]
    fn anchors_do_not_self_seed() {
        // A single anchor with no neighbors above threshold must end up
        // with activation 0 for itself -- activation is energy *received*,
        // not a reflection of the anchor's own seed score.
        let embs = vec![vec![1.0_f32, 0.0]];
        let scores = vec![0.9];
        let act = diffuse_activation(&embs, &scores, &[], &cfg(true));
        assert_eq!(act, vec![0.0]);
    }

    #[test]
    fn deterministic_across_repeat_calls() {
        let embs = vec![
            vec![1.0_f32, 0.0, 0.0],
            vec![0.9_f32, 0.3, 0.0],
            vec![0.0_f32, 1.0, 0.0],
            vec![0.0_f32, 0.0, 1.0],
        ];
        let scores = vec![0.5, 0.2, 0.2, 0.2];
        let a1 = diffuse_activation(&embs, &scores, &[], &cfg(true));
        let a2 = diffuse_activation(&embs, &scores, &[], &cfg(true));
        assert_eq!(a1, a2);
    }
}
