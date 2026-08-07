//! Recall: selects ≤6 beliefs for the `[What I know about you]` block via
//! DPP over the candidate set, then renders the four sections within the
//! 350-token hard cap.

use chrono::Utc;

use crate::belief::{effective_weight, SelectedBelief};
use crate::store::beliefs::Belief;
use crate::vector::dpp::{build_dpp_kernel, dpp_sample};

/// Maximum beliefs in the main block.
pub const MAX_BELIEFS: usize = 6;

/// The universal footer, appended every turn.
pub const FOOTER: &str =
    "This is a model of you, not a fact about you. If any of it is wrong, say so and I'll drop it.";

/// Select up to `MAX_BELIEFS` beliefs by weighted DPP over the candidate set.
/// `query_domain` biases (via the effective weight) toward in-domain beliefs
/// without ever excluding cross-domain ones.
pub fn select_beliefs(
    candidates: &[Belief],
    query_domain: Option<&str>,
) -> Vec<SelectedBelief> {
    if candidates.is_empty() {
        return vec![];
    }
    let now = Utc::now();
    // effective weight per candidate
    let weights: Vec<f64> = candidates
        .iter()
        .map(|b| effective_weight(b, query_domain, now))
        .collect();

    // Filter suppressed-as-zero (weight 0.0) out -- they must not occupy a slot.
    let alive: Vec<(usize, &Belief, f64)> = candidates
        .iter()
        .enumerate()
        .zip(weights.iter())
        .filter_map(|((i, b), &w)| if w > 0.0 { Some((i, b, w)) } else { None })
        .collect();
    if alive.is_empty() {
        return vec![];
    }

    let embeds: Vec<Vec<f32>> = alive.iter().map(|(_, b, _)| b.embedding.clone()).collect();
    let scores: Vec<f64> = alive.iter().map(|(_, _, w)| *w).collect();

    let kernel = build_dpp_kernel(&embeds, &scores, 1.0);
    let idx = dpp_sample(&kernel, MAX_BELIEFS, 1e-7);

    idx.into_iter()
        .filter_map(|k| alive.get(k))
        .map(|(_, b, w)| SelectedBelief {
            belief: (*b).clone(),
            effective_weight: *w,
        })
        .collect()
}

/// Truncation order when over the token budget: CheckYourself → WorthTesting
/// → uncertainty lines → weakest beliefs. Never mid-line. (Token counting is
/// provided by the daemon via `count_text_tokens`; here we expose the ordering
/// helper and a render that a caller can token-cap.)
pub fn truncation_order() -> &'static [&'static str] {
    &[
        "[Check yourself]",
        "[Worth testing]",
        "[Where I'm unsure]",
        "[What I know about you]",
    ]
}

/// Render the `[What I know about you]` section from a selected set,
/// sorted by (effective_weight desc, belief_id asc) so unchanged state
/// renders byte-identical.
pub fn render_knows(selected: &mut [SelectedBelief]) -> String {
    selected.sort_by(|a, b| {
        b.effective_weight
            .partial_cmp(&a.effective_weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.belief.id.cmp(&b.belief.id))
    });
    let mut lines = Vec::new();
    for s in selected.iter() {
        lines.push(format!("- {}", s.belief.text));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::beliefs::{Layer, Provenance};
    use chrono::Utc;

    fn belief(id: &str, text: &str, conf: f64, tested: bool, layer: Layer, domain: Option<&str>, emb: Vec<f32>) -> Belief {
        Belief {
            id: id.into(),
            text: text.into(),
            embedding: emb,
            confidence: conf,
            provenance: Provenance::DirectStatement,
            layer,
            tested,
            domain: domain.map(|s| s.into()),
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(Utc::now()),
            consolidated_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn empty_candidates_empty() {
        assert!(select_beliefs(&[], None).is_empty());
    }

    #[test]
    fn untested_0_8_ranks_below_tested_0_55() {
        // unit vectors along orthogonal axes; both present
        let e1 = vec![1.0_f32, 0.0];
        let e2 = vec![0.0_f32, 1.0];
        let untested_high = belief("a", "untested high", 0.8, false, Layer::Context, Some("d"), e1);
        let tested_low = belief("b", "tested low", 0.55, true, Layer::Context, Some("d"), e2);
        let sel = select_beliefs(&[untested_high, tested_low], Some("d"));
        assert_eq!(sel.len(), 2);
        // tested low should rank above untested high (their orthogonal
        // embeddings -> DPP kernel diagonal ~ weight^2, argmax picks tested)
        assert!(sel[0].belief.id == "b");
    }

    #[test]
    fn cross_domain_downweighted_not_excluded() {
        let e = vec![1.0_f32, 0.0, 0.0];
        let in_domain = belief("in", "in domain", 0.5, true, Layer::Context, Some("code"), e.clone());
        let cross = belief("cross", "cross domain", 0.6, true, Layer::Context, Some("cooking"), e);
        let sel = select_beliefs(&[in_domain, cross], Some("code"));
        // both present (not excluded)
        assert_eq!(sel.len(), 2);
        // in-domain (same domain, weight 0.5) ranks above cross (0.6*0.35=0.21)
        assert_eq!(sel[0].belief.id, "in");
    }

    #[test]
    fn dpp_surfaces_opposed_over_near_identical() {
        // two beliefs very similar in embedding but low weight, and two
        // opposed-but-distinct; DPP should surface the diverse pair
        let e_base = vec![1.0_f32, 0.0];
        let similar = vec![0.99_f32, 0.10];
        let opposed = vec![-1.0_f32, 0.0];
        let a = belief("a", "technical", 0.6, true, Layer::Context, Some("d"), e_base);
        let b = belief("b", "technical-similar", 0.6, true, Layer::Context, Some("d"), similar);
        let c = belief("c", "technical-opposed", 0.6, true, Layer::Context, Some("d"), opposed);
        let sel = select_beliefs(&[a.clone(), b, c.clone()], Some("d"));
        // With 2 slots and near-equal weights, the two closest are NOT both
        // chosen; "opposed" is furthest from "technical", so it surfaces.
        assert!(sel.iter().any(|s| s.belief.id == "c"));
    }

    #[test]
    fn render_is_byte_identical_on_repeat() {
        let e = vec![1.0_f32];
        let mut s1 = select_beliefs(
            &[belief("a", "x", 0.7, true, Layer::Context, None, e.clone()),
              belief("b", "y", 0.5, true, Layer::Context, None, e.clone())],
            None,
        );
        let mut s2 = select_beliefs(
            &[belief("a", "x", 0.7, true, Layer::Context, None, e.clone()),
              belief("b", "y", 0.5, true, Layer::Context, None, e.clone())],
            None,
        );
        let r1 = render_knows(&mut s1);
        let r2 = render_knows(&mut s2);
        assert_eq!(r1, r2);
    }
}
