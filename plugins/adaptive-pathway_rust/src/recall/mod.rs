//! Recall: selects ≤6 beliefs for the `[What I know about you]` block via
//! DPP over the candidate set, then renders the four sections within the
//! 350-token hard cap.

use chrono::Utc;

use crate::belief::{effective_weight, SelectedBelief};
use crate::store::beliefs::Belief;
use crate::vector::dpp::{build_dpp_kernel, dpp_sample};
use crate::vector::ops;

/// Maximum beliefs in the main block.
pub const MAX_BELIEFS: usize = 6;

/// Maximum candidates fed into the DPP kernel. Caps the O(N²) kernel build +
/// rank-1 downdate at a fixed bound regardless of how large the belief store
/// grows, so recall cost stays constant per turn.
pub const MAX_CANDIDATES: usize = 64;

/// The universal footer, appended every turn.
pub const FOOTER: &str =
    "This is a model of you, not a fact about you. If any of it is wrong, say so and I'll drop it.";

/// Select up to `MAX_BELIEFS` beliefs by weighted DPP over the candidate set.
/// `query_domain` biases (via the effective weight) toward in-domain beliefs
/// without ever excluding cross-domain ones. No query relevance; equivalent to
/// `select_beliefs_relevant` with an empty query vector.
pub fn select_beliefs(
    candidates: &[Belief],
    query_domain: Option<&str>,
) -> Vec<SelectedBelief> {
    select_beliefs_relevant(candidates, &[], query_domain)
}

/// Select up to `MAX_BELIEFS` beliefs by weighted DPP, grounding the choice in
/// the current user query and bounding the DPP cost:
///   1. Score every candidate by `effective_weight × (0.5 + 0.5·cosine(query,
///      belief))` so semantically-relevant beliefs are favoured (query may be
///      the zero/empty vector for pure weight-driven selection).
///   2. Cap the candidate pool at `MAX_CANDIDATES` by that blended score so the
///      O(N²) DPP kernel stays bounded as the belief store grows.
///   3. Drop suppressed-as-zero (weight 0.0) candidates, then DPP over the cap.
pub fn select_beliefs_relevant(
    candidates: &[Belief],
    query: &[f32],
    query_domain: Option<&str>,
) -> Vec<SelectedBelief> {
    if candidates.is_empty() {
        return vec![];
    }
    let now = Utc::now();
    let has_query = ops::norm(query) > 1e-12;

    // (idx, belief, effective_weight, blended_score)
    let mut cand: Vec<(usize, &Belief, f64, f64)> = candidates
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let w = effective_weight(b, query_domain, now);
            let rel = if has_query {
                ops::cosine(query, &b.embedding).max(0.0)
            } else {
                0.0
            };
            (i, b, w, w * (0.5 + 0.5 * rel))
        })
        .collect();

    // Cap by blended score (ties broken by original index for determinism).
    if cand.len() > MAX_CANDIDATES {
        cand.sort_by(|a, b| {
            b.3.partial_cmp(&a.3)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        cand.truncate(MAX_CANDIDATES);
    }

    // Filter suppressed-as-zero (weight 0.0) out -- they must not occupy a slot.
    // The blended score is used as the DPP weight so query relevance actually
    // drives selection (an empty query scales all scores uniformly, which does
    // not change DPP selection, so `select_beliefs` is unaffected). The true
    // effective weight is reported on `SelectedBelief`.
    let alive: Vec<(usize, &Belief, f64, f64)> = cand
        .into_iter()
        .filter(|(_, _, w, _)| *w > 0.0)
        .collect();
    if alive.is_empty() {
        return vec![];
    }

    let embeds: Vec<Vec<f32>> = alive.iter().map(|(_, b, _, _)| b.embedding.clone()).collect();
    let scores: Vec<f64> = alive.iter().map(|(_, _, _, s)| *s).collect();

    let kernel = build_dpp_kernel(&embeds, &scores, 1.0);
    let idx = dpp_sample(&kernel, MAX_BELIEFS, 1e-7);

    idx.into_iter()
        .filter_map(|k| alive.get(k))
        .map(|(_, b, w, _)| SelectedBelief {
            belief: (*b).clone(),
            effective_weight: *w,
        })
        .collect()
}

/// Truncation order when over the token budget: CheckYourself → WorthTesting
/// → uncertainty lines → weakest beliefs. Never mid-line. Labels are the
/// exact section headers `antisycophancy::render_block` emits (matched via
/// `starts_with` against each joined section, since `[Check yourself]`'s
/// header and body are one string, not header-then-newline-then-body like
/// the other three).
pub fn truncation_order() -> &'static [&'static str] {
    &[
        "[Check yourself]",
        "[Worth testing this turn]",
        "[Where I'm unsure]",
        "[What I know about you]",
    ]
}

/// Hard cap on the rendered recall block, injected into every turn's
/// prompt. ~200 tokens typical, 350 the ceiling -- meaningful against a
/// 64k context window, small against the summarizer's compaction threshold.
pub const RECALL_MAX_TOKENS: usize = 350;

/// Rough token estimate (~4 chars/token). This crate has no real tokenizer
/// and deliberately doesn't depend on the daemon's `count_text_tokens`
/// (different crate; the `StructuredChat` trait-inversion pattern exists
/// for exactly this kind of cross-crate need, but a token *count* isn't
/// worth that ceremony for a soft budget check).
fn estimate_tokens(s: &str) -> usize {
    (s.chars().count() + 3) / 4
}

/// Enforce the recall token budget by dropping whole sections in
/// `truncation_order()` -- `[Check yourself]` first, then `[Worth testing
/// this turn]`, then `[Where I'm unsure]`, then `[What I know about you]`
/// last -- never truncating mid-line. `block` must already be the full
/// joined text from `antisycophancy::render_block` (sections separated by
/// `"\n\n"`, footer last).
pub fn cap_to_token_budget(block: String) -> String {
    if estimate_tokens(&block) <= RECALL_MAX_TOKENS {
        return block;
    }
    let mut sections: Vec<&str> = block.split("\n\n").collect();
    for header in truncation_order() {
        if estimate_tokens(&sections.join("\n\n")) <= RECALL_MAX_TOKENS {
            break;
        }
        sections.retain(|s| !s.starts_with(header));
    }
    let mut joined = sections.join("\n\n");
    // Last resort: even `[What I know about you]` alone (e.g. one very long
    // belief line) can't be dropped without losing the whole block's point,
    // so hard-truncate at a char boundary rather than exceeding the budget.
    if estimate_tokens(&joined) > RECALL_MAX_TOKENS {
        let max_chars = RECALL_MAX_TOKENS * 4;
        if joined.chars().count() > max_chars {
            joined = joined.chars().take(max_chars).collect();
        }
    }
    joined
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
            session_id: None,
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

    #[test]
    fn relevance_grounds_selection_in_the_query() {
        // Two beliefs of equal (strong) weight, one coaxial with the query,
        // one orthogonal. Without a query both survive with the same weight;
        // with a query along axis 0 the blended DPP weight must pull the
        // relevant belief to the front.
        let e0 = vec![1.0_f32, 0.0, 0.0];
        let e1 = vec![0.0_f32, 1.0, 0.0];
        let a = belief("a", "on-axis", 0.7, true, Layer::Context, Some("d"), e0.clone());
        let b = belief("b", "off-axis", 0.7, true, Layer::Context, Some("d"), e1);
        // Empty query: both kept, deterministic (order by DPP argmax).
        let noq = select_beliefs_relevant(&[a.clone(), b.clone()], &[], None);
        assert_eq!(noq.len(), 2);
        // Query on axis 0: the coaxial belief carries more weight into DPP, so
        // it must be selected first (slot 0).
        let sel = select_beliefs_relevant(&[a.clone(), b.clone()], &e0, None);
        assert_eq!(sel[0].belief.id, "a");
    }

    #[test]
    fn candidate_cap_drops_low_relevance_when_pool_exceeds_budget() {
        // A store larger than MAX_CANDIDATES must be reduced before DPP (the
        // O(N²) kernel stays bounded), be deterministic, and surface a
        // query-relevant belief. DPP still diversifies across the many
        // identical on-axis beliefs, so we assert bounds + determinism +
        // inclusion, not that every slot is relevant.
        let mut cands: Vec<Belief> = Vec::new();
        let on_axis = vec![1.0_f32, 0.0, 0.0];
        for i in 0..30 {
            cands.push(belief(&format!("on{i}"), &format!("on {i}"), 0.5, true, Layer::Context, Some("d"), on_axis.clone()));
        }
        let off_axis = vec![0.0_f32, 1.0, 0.0];
        for i in 0..(MAX_CANDIDATES + 10) {
            cands.push(belief(&format!("off{i}"), &format!("off {i}"), 0.5, true, Layer::Context, Some("d"), off_axis.clone()));
        }
        let sel = select_beliefs_relevant(&cands, &on_axis, None);
        assert!(!sel.is_empty() && sel.len() <= MAX_BELIEFS);
        // The top-relevance belief survives the cap + selection.
        assert!(sel.iter().any(|s| s.belief.id == "on0"));
        // Determinism (ties broken by index + greedy DPP are stable).
        let again = select_beliefs_relevant(&cands, &on_axis, None);
        let ids: Vec<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        let ids2: Vec<&str> = again.iter().map(|s| s.belief.id.as_str()).collect();
        assert_eq!(ids, ids2);
    }
}
