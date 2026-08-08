//! Recall: selects ≤6 beliefs for the `[Working assumptions about you]`
//! block via DPP over the candidate set, then renders the four sections
//! within the 350-token hard cap.
//!
//! Two renderings share that selection: `render_knows` +
//! `antisycophancy::render_block` for the labeled system-block path, and
//! `render_reflection` + `render_reflection_block` for the `<think>`
//! thought-seed path. Both frame recalled beliefs as a provisional prior to
//! test the current request against -- never as a profile to conform to.
//! Changing one framing without the other is a bug: the seed path fires only
//! for prefill-capable reasoning models, so the system-block path is what
//! local Ollama actually sees.

use chrono::Utc;

use crate::belief::{effective_weight, SelectedBelief};
use crate::config::Config;
use crate::store::beliefs::Belief;
use crate::vector::dpp::{build_dpp_kernel_from_normalized, dpp_sample};
use crate::vector::ops;
use crate::vector::spread::diffuse_activation;

/// Maximum beliefs in the main block.
pub const MAX_BELIEFS: usize = 6;

/// Slots held for `Layer::Identity` beliefs before the general pool competes
/// for the rest.
///
/// Identity beliefs are the slowest-moving thing the engine knows (365-day
/// half-life, `layers.rs`) and are only ever reached by promotion through
/// `consolidate.rs`'s gates — but they compete for a slot on raw effective
/// weight like everything else, so a burst of fresh, highly-relevant
/// conversation beliefs can evict all of them from a turn. Reserving a
/// couple of slots is the cheap version of a separate always-on profile
/// block, and deliberately *not* the expensive version: a static profile
/// injected unconditionally every turn is the single most sycophancy-inducing
/// shape recall can take, because the model pattern-matches to it. Identity
/// beliefs stay inside the same "working assumptions, correct me" framing as
/// everything else — they just can't be crowded out of it.
///
/// Unused when the candidate set has no identity beliefs: the reserved pass
/// simply selects nothing and the general pass takes all `MAX_BELIEFS`.
pub const IDENTITY_RESERVED: usize = 2;

/// Maximum candidates fed into the DPP kernel. Caps the O(N²) kernel build +
/// rank-1 downdate at a fixed bound regardless of how large the belief store
/// grows, so recall cost stays constant per turn.
pub const MAX_CANDIDATES: usize = 64;

/// The universal footer, appended every turn.
///
/// Deliberately dialectical rather than descriptive: the earlier wording
/// ("this is a model of you...") described the block's epistemic status but
/// gave no instruction about what to *do* with it, which in practice reads
/// as licence to reason forward from the profile -- i.e. to conform. This
/// version tells the model to check the recalled beliefs against the actual
/// request and to name a conflict rather than work around it, in both
/// directions (stale belief, or shaky premise in the request itself).
pub const FOOTER: &str =
    "These are inferences from earlier turns, not facts, and some are probably stale. \
     Check them against what's actually being asked rather than reasoning forward from \
     them -- and if the request itself rests on something shaky, say so plainly instead \
     of working around it.";

/// Select up to `MAX_BELIEFS` beliefs by weighted DPP over the candidate set.
/// `query_domain` biases (via the effective weight) toward in-domain beliefs
/// without ever excluding cross-domain ones. No query relevance; equivalent to
/// `select_beliefs_relevant` with an empty query vector.
pub fn select_beliefs(
    candidates: &[Belief],
    query_domain: Option<&str>,
    cfg: &Config,
) -> Vec<SelectedBelief> {
    select_beliefs_relevant(candidates, &[], query_domain, &[], cfg)
}

/// Select up to `MAX_BELIEFS` beliefs by weighted DPP, grounding the choice in
/// the current user query and bounding the DPP cost:
///   1. Score every candidate by `effective_weight × (0.5 + 0.5·cosine(query,
///      belief))` so semantically-relevant beliefs are favoured (query may be
///      the zero/empty vector for pure weight-driven selection).
///   2. Cap the candidate pool at `MAX_CANDIDATES` by that blended score so the
///      O(N²) DPP kernel stays bounded as the belief store grows.
///   3. Drop suppressed-as-zero (weight 0.0) candidates.
///   4. Diffuse spreading activation over the survivors' cosine *and*
///      co-occurrence graph (`vector::spread::diffuse_activation`,
///      config-gated by `cfg.diffusion`) and fold it into each candidate's
///      DPP input score, then DPP over the result. Embeddings are normalized
///      once here and reused for both the diffusion graph and the DPP kernel,
///      rather than cloning+renormalizing twice.
///
/// `cooccurrence` is an adjacency list over `candidates` *by original index*
/// — beliefs observed together in one extraction batch. Callers that don't
/// have it (or don't want it) pass `&[]`, which reduces step 4 to pure cosine
/// diffusion. It's taken as plain data rather than fetched here so this stays
/// a pure, synchronously-testable function; `engine::select_for_turn` does
/// the DB read.
pub fn select_beliefs_relevant(
    candidates: &[Belief],
    query: &[f32],
    query_domain: Option<&str>,
    cooccurrence: &[Vec<usize>],
    cfg: &Config,
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

    let scores: Vec<f64> = alive.iter().map(|(_, _, _, s)| *s).collect();

    // Normalized once, reused for both the diffusion graph and the DPP
    // kernel (previously cloned+renormalized separately for each).
    let mut normed: Vec<Vec<f32>> = alive.iter().map(|(_, b, _, _)| b.embedding.clone()).collect();
    for e in normed.iter_mut() {
        ops::normalize_in_place(e);
    }

    // Remap the caller's adjacency (original candidate indices) onto `alive`
    // positions -- the pool has been score-capped and weight-filtered since,
    // so the two index spaces are not the same. Siblings that didn't survive
    // simply drop out of the graph.
    let cooccurrence_alive: Vec<Vec<usize>> = if cooccurrence.is_empty() {
        Vec::new()
    } else {
        let mut position_of = std::collections::HashMap::with_capacity(alive.len());
        for (pos, (orig, _, _, _)) in alive.iter().enumerate() {
            position_of.insert(*orig, pos);
        }
        alive
            .iter()
            .map(|(orig, _, _, _)| {
                cooccurrence
                    .get(*orig)
                    .map(|siblings| {
                        siblings.iter().filter_map(|s| position_of.get(s).copied()).collect()
                    })
                    .unwrap_or_default()
            })
            .collect()
    };

    let activation = diffuse_activation(&normed, &scores, &cooccurrence_alive, &cfg.diffusion);
    let diffused_scores: Vec<f64> = scores
        .iter()
        .zip(activation.iter())
        .map(|(&s, &a)| s * (1.0 + cfg.diffusion.boost_weight * a))
        .collect();

    let kernel =
        build_dpp_kernel_from_normalized(&normed, &diffused_scores, cfg.dpp.default_diversity_weight);

    // Two restricted greedy passes rather than one pass plus a post-hoc swap.
    // DPP's rank-1 downdate is sequential and order-dependent, so swapping a
    // pick out afterwards would leave the remaining selections conditioned on
    // an item that's no longer there; restricting the candidate set up front
    // keeps each pass internally consistent and the whole thing deterministic
    // by construction. See `IDENTITY_RESERVED`.
    //
    // Known limitation, accepted: the two passes don't diversify against each
    // other, so a general-pool pick could in principle near-duplicate a
    // reserved identity pick. Conditioning the second pass on the first's
    // downdates would need `dpp_sample` to take pre-selected items, and the
    // case is largely already prevented upstream -- observations within
    // MERGE_COSINE (0.86) of an existing belief merge into it at write time
    // rather than becoming a second, near-identical row.
    let identity_positions: Vec<usize> = alive
        .iter()
        .enumerate()
        .filter(|(_, (_, b, _, _))| b.layer == crate::store::beliefs::Layer::Identity)
        .map(|(pos, _)| pos)
        .collect();

    let mut idx: Vec<usize> = if identity_positions.is_empty() {
        dpp_sample(&kernel, MAX_BELIEFS, cfg.dpp.epsilon)
    } else {
        let reserved = IDENTITY_RESERVED.min(identity_positions.len());
        let mut picked = dpp_sample_restricted(&kernel, &identity_positions, reserved, cfg.dpp.epsilon);
        let rest: Vec<usize> = (0..alive.len()).filter(|p| !picked.contains(p)).collect();
        picked.extend(dpp_sample_restricted(
            &kernel,
            &rest,
            MAX_BELIEFS.saturating_sub(picked.len()),
            cfg.dpp.epsilon,
        ));
        picked
    };
    idx.truncate(MAX_BELIEFS);

    idx.into_iter()
        .filter_map(|k| alive.get(k))
        .map(|(_, b, w, _)| SelectedBelief {
            belief: (*b).clone(),
            effective_weight: *w,
        })
        .collect()
}

/// `dpp_sample` over a subset of the kernel's items, returning indices in the
/// *original* kernel space. Builds the submatrix rather than masking in place
/// so `dpp_sample`'s greedy loop and downdate stay untouched — the diversity
/// math is identical, it just sees fewer candidates.
fn dpp_sample_restricted(
    kernel: &[Vec<f64>],
    positions: &[usize],
    k: usize,
    epsilon: f64,
) -> Vec<usize> {
    if positions.is_empty() || k == 0 {
        return vec![];
    }
    let sub: Vec<Vec<f64>> = positions
        .iter()
        .map(|&i| positions.iter().map(|&j| kernel[i][j]).collect())
        .collect();
    dpp_sample(&sub, k, epsilon)
        .into_iter()
        .filter_map(|local| positions.get(local).copied())
        .collect()
}

/// Truncation order when over the token budget: CheckYourself → WorthTesting
/// → uncertainty lines → weakest beliefs. Never mid-line. Labels are the
/// exact section headers `antisycophancy::render_block` emits, matched via
/// `starts_with` against each `"\n\n"`-joined section. All four now share
/// the same `header\nbody` shape (`[Check yourself]` used to bake its label
/// into the body string; `render_block` applies it uniformly instead).
///
/// `render_reflection_block` deliberately emits no headers -- a `<think>`
/// prefill that carries bracketed scaffolding reads as injected boilerplate
/// rather than the model's own recollection -- so it gets its own
/// positional equivalent, `cap_reflection_to_token_budget`, whose
/// drop-from-the-end order is kept in sync with this list.
pub fn truncation_order() -> &'static [&'static str] {
    &[
        "[Check yourself]",
        "[Worth testing this turn]",
        "[Where I'm unsure]",
        "[Working assumptions about you]",
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
/// this turn]`, then `[Where I'm unsure]`, then `[Working assumptions about you]`
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
    // Last resort: even `[Working assumptions about you]` alone (e.g. one very long
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

/// Render the `[Working assumptions about you]` section from a selected set,
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

/// Terse first-person reflection rendering for thought-seeding
/// (`PathwayEngine::recall_thought_seed`) -- prefilled into a trailing
/// `<think>` turn instead of a labeled `[Working assumptions about you]`
/// system block, so it reads as the model's own recollection rather than an
/// injected fact sheet. No section headers (see `truncation_order`'s note),
/// and no `FOOTER` verbatim -- the footer's instruction is folded into this
/// sentence in first person instead, since a `<think>` turn addressed in
/// the second person gives the seam away.
///
/// The framing is deliberately dialectical. The original wording ended "I'll
/// let that inform my tone without stating it outright", which is a
/// conformity instruction: it told the model to silently shape itself to the
/// stored profile, making the seeded path *more* sycophantic than the system
/// block it replaces. Recalled beliefs are presented as a prior to test the
/// request against, not a mold to fit.
///
/// Same (effective_weight desc, belief_id asc) sort as `render_knows`, for
/// the same byte-stability reason. Empty input renders empty, same "caller
/// treats `None`/empty as zero prompt delta" contract as everything else in
/// recall.
pub fn render_reflection(selected: &mut [SelectedBelief]) -> String {
    if selected.is_empty() {
        return String::new();
    }
    selected.sort_by(|a, b| {
        b.effective_weight
            .partial_cmp(&a.effective_weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.belief.id.cmp(&b.belief.id))
    });
    let facts: Vec<&str> = selected.iter().map(|s| s.belief.text.as_str()).collect();
    format!(
        "What I think I've picked up about this person so far: {}. \
         That's inference from earlier turns, not fact, and some of it is probably \
         stale -- I should check it against what they're actually asking rather than \
         reasoning forward from it. If the request itself rests on something shaky, \
         better to name that than quietly work around it.",
        facts.join("; ")
    )
}

/// Assemble the full thought-seed block: the reflection plus the same three
/// anti-sycophancy signals `antisycophancy::render_block` carries, in
/// inner-monologue voice with no bracketed headers.
///
/// This exists because `recall_thought_seed` originally rendered *only* the
/// fact list, silently dropping `[Worth testing this turn]`,
/// `[Where I'm unsure]` and `[Check yourself]` -- exactly the machinery that
/// makes recall a thought-partnership signal rather than a profile to match.
/// The three inputs are already first-person sentences at their source
/// (`belief::lifecycle::test_prompt`, `engine::unsure_line`,
/// `antisycophancy::check_yourself`), so they drop into a `<think>` turn
/// unchanged.
///
/// Part order is the reverse of `truncation_order()`, so
/// `cap_reflection_to_token_budget`'s drop-from-the-end policy sheds the
/// same sections in the same priority as the system-block path.
pub fn render_reflection_block(
    reflection: &str,
    worth_testing: Option<String>,
    unsure: Option<String>,
    check: Option<String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !reflection.is_empty() {
        parts.push(reflection.to_string());
    }
    for part in [unsure, worth_testing, check].into_iter().flatten() {
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join("\n\n")
}

/// `cap_to_token_budget`'s equivalent for the header-less thought-seed
/// block: drops whole trailing paragraphs (which `render_reflection_block`
/// orders least-important-last) until the budget is met, then hard-truncates
/// as a last resort exactly like the system-block path.
pub fn cap_reflection_to_token_budget(block: String) -> String {
    if estimate_tokens(&block) <= RECALL_MAX_TOKENS {
        return block;
    }
    let mut sections: Vec<&str> = block.split("\n\n").collect();
    while sections.len() > 1 && estimate_tokens(&sections.join("\n\n")) > RECALL_MAX_TOKENS {
        sections.pop();
    }
    let mut joined = sections.join("\n\n");
    if estimate_tokens(&joined) > RECALL_MAX_TOKENS {
        let max_chars = RECALL_MAX_TOKENS * 4;
        if joined.chars().count() > max_chars {
            joined = joined.chars().take(max_chars).collect();
        }
    }
    joined
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
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
        }
    }

    #[test]
    fn empty_candidates_empty() {
        assert!(select_beliefs(&[], None, &Config::default()).is_empty());
    }

    #[test]
    fn untested_0_8_ranks_below_tested_0_55() {
        // unit vectors along orthogonal axes; both present
        let e1 = vec![1.0_f32, 0.0];
        let e2 = vec![0.0_f32, 1.0];
        let untested_high = belief("a", "untested high", 0.8, false, Layer::Context, Some("d"), e1);
        let tested_low = belief("b", "tested low", 0.55, true, Layer::Context, Some("d"), e2);
        let sel = select_beliefs(&[untested_high, tested_low], Some("d"), &Config::default());
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
        let sel = select_beliefs(&[in_domain, cross], Some("code"), &Config::default());
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
        let sel = select_beliefs(&[a.clone(), b, c.clone()], Some("d"), &Config::default());
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
            &Config::default(),
        );
        let mut s2 = select_beliefs(
            &[belief("a", "x", 0.7, true, Layer::Context, None, e.clone()),
              belief("b", "y", 0.5, true, Layer::Context, None, e.clone())],
            None,
            &Config::default(),
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
        let noq = select_beliefs_relevant(&[a.clone(), b.clone()], &[], None, &[], &Config::default());
        assert_eq!(noq.len(), 2);
        // Query on axis 0: the coaxial belief carries more weight into DPP, so
        // it must be selected first (slot 0).
        let sel = select_beliefs_relevant(&[a.clone(), b.clone()], &e0, None, &[], &Config::default());
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
        let sel = select_beliefs_relevant(&cands, &on_axis, None, &[], &Config::default());
        assert!(!sel.is_empty() && sel.len() <= MAX_BELIEFS);
        // The top-relevance belief survives the cap + selection.
        assert!(sel.iter().any(|s| s.belief.id == "on0"));
        // Determinism (ties broken by index + greedy DPP are stable).
        let again = select_beliefs_relevant(&cands, &on_axis, None, &[], &Config::default());
        let ids: Vec<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        let ids2: Vec<&str> = again.iter().map(|s| s.belief.id.as_str()).collect();
        assert_eq!(ids, ids2);
    }
}
