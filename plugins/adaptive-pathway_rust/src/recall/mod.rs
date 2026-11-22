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
pub mod cut_log;

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
    select_beliefs_relevant_with(candidates, query, query_domain, cooccurrence, None, cfg)
}

/// A host judge's view of how relevant some candidates are to the turn, keyed
/// by candidate index (the same index space as `cooccurrence`).
#[derive(Debug, Clone, Default)]
pub struct Relevance {
    pub by_index: std::collections::HashMap<usize, f32>,
    /// A judged candidate below this is not injected -- unless it is an
    /// identity belief, whose reserved slots never depend on the judge.
    pub drop_below: f32,
}

/// Component-wise mean of the candidate embeddings — the corpus mean removed
/// before the centered-cosine relevance gate (`RecallConfig`). `None` when
/// there are no non-empty embeddings to average.
pub(crate) fn corpus_mean(candidates: &[Belief]) -> Option<Vec<f32>> {
    let dim = candidates.iter().map(|b| b.embedding.len()).find(|&n| n > 0)?;
    let mut sum = vec![0.0f64; dim];
    let mut n = 0usize;
    for b in candidates {
        if b.embedding.len() != dim {
            continue;
        }
        for (s, &x) in sum.iter_mut().zip(b.embedding.iter()) {
            *s += x as f64;
        }
        n += 1;
    }
    if n == 0 {
        return None;
    }
    Some(sum.iter().map(|s| (s / n as f64) as f32).collect())
}

/// Elementwise `a - b` (the centering step); a shorter `b` leaves the tail of
/// `a` unchanged.
pub(crate) fn sub(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter()
        .enumerate()
        .map(|(i, &x)| x - b.get(i).copied().unwrap_or(0.0))
        .collect()
}

/// `(index, belief, effective_weight, blended_score, centered_relevance)` for
/// every candidate. `centered_relevance` is the mean-centered cosine of the
/// query against the belief (0.0 when there is no query); it drives the
/// relevance floor in [`select_beliefs_relevant_with`]. The blended *score*
/// (the DPP/ordering weight) folds in only the non-negative part, so an
/// irrelevant belief can be dropped by the floor without ever producing a
/// non-positive DPP weight.
fn blended<'a>(
    candidates: &'a [Belief],
    query: &[f32],
    query_domain: Option<&str>,
) -> Vec<(usize, &'a Belief, f64, f64, f64)> {
    let now = Utc::now();
    let has_query = ops::norm(query) > 1e-12;
    let mean = if has_query { corpus_mean(candidates) } else { None };
    let qc = mean.as_ref().map(|m| sub(query, m));
    candidates
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let w = effective_weight(b, query_domain, now);
            // Mean-centered cosine so relevance has real dynamic range in a
            // space where raw cosine is compressed near 1 (see `RecallConfig`).
            let rel = match (&qc, &mean) {
                (Some(qc), Some(m)) if m.len() == b.embedding.len() => {
                    ops::cosine(qc, &sub(&b.embedding, m))
                }
                _ => 0.0,
            };
            (i, b, w, w * (0.5 + 0.5 * rel.clamp(0.0, 1.0)), rel)
        })
        .collect()
}

/// The indices of the `n` strongest live candidates by the same blended score
/// selection starts from -- the ones worth asking a judge about.
pub fn preselect(candidates: &[Belief], query: &[f32], query_domain: Option<&str>, n: usize) -> Vec<usize> {
    let mut cand = blended(candidates, query, query_domain);
    cand.retain(|(_, _, w, _, _)| *w > 0.0);
    cand.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    cand.into_iter().take(n).map(|(i, _, _, _, _)| i).collect()
}

/// [`select_beliefs_relevant`] with an optional judge's relevance folded into
/// the blended score before anything else: a judged belief's score is scaled
/// by `0.3 + 0.7 * p`, and one below `drop_below` leaves the pool (identity
/// beliefs excepted) so DPP backfills from the rest. `None` is exactly the
/// unjudged selection.
pub fn select_beliefs_relevant_with(
    candidates: &[Belief],
    query: &[f32],
    query_domain: Option<&str>,
    cooccurrence: &[Vec<usize>],
    relevance: Option<&Relevance>,
    cfg: &Config,
) -> Vec<SelectedBelief> {
    select_beliefs_relevant_with_cut(candidates, query, query_domain, cooccurrence, relevance, cfg).0
}

/// [`select_beliefs_relevant_with`] that also returns this turn's **effective
/// cut**: the lowest centered relevance among the general (non-pinned,
/// non-identity) beliefs that survived the floor + per-query gap cut, or `None`
/// when none did. The async caller (`engine::select_for_turn`) folds it into
/// the running EWMA that raises the operational floor over time. Pure and
/// synchronously testable; the EWMA read/write stays in the engine.
pub fn select_beliefs_relevant_with_cut(
    candidates: &[Belief],
    query: &[f32],
    query_domain: Option<&str>,
    cooccurrence: &[Vec<usize>],
    relevance: Option<&Relevance>,
    cfg: &Config,
) -> (Vec<SelectedBelief>, Option<f64>) {
    if candidates.is_empty() {
        return (vec![], None);
    }

    // (idx, belief, effective_weight, blended_score, centered_relevance)
    let mut cand = blended(candidates, query, query_domain);
    let mut effective_cut: Option<f64> = None;

    // Deterministic centered-cosine relevance gate (`RecallConfig`), applied
    // every turn regardless of the optional LLM judge below. With a query
    // present, drop beliefs whose centered relevance is under the floor;
    // pinned beliefs are exempt and identity beliefs use the gentler identity
    // floor (so a stale profile belief no longer injects on an unrelated
    // turn). An empty query has no cosine, so the gate is a no-op there —
    // `select_beliefs` stays weight-only.
    let has_query = ops::norm(query) > 1e-12;
    if has_query {
        let floor = cfg.recall.min_cosine;
        let floor_identity = cfg.recall.min_cosine_identity;
        // Identity and the standing profile are judged against the gentler
        // identity floor: they still have to bear on the turn, only less
        // closely than a topical belief. Pinned beliefs alone are exempt.
        let is_identity =
            |b: &Belief| b.layer == crate::store::beliefs::Layer::Identity || is_standing_profile(b);
        let is_exempt = |b: &Belief| b.pinned;
        cand.retain(|(_, b, _, _, rel)| {
            is_exempt(b)
                || if is_identity(b) {
                    *rel >= floor_identity
                } else {
                    *rel >= floor
                }
        });

        // Per-query gap cut over the general (non-exempt) survivors: keep only
        // the top relevance cluster for this turn, dropping a weak tail that
        // cleared the flat floor but sits well below what the query actually
        // matched. Pinned, standing-profile, and identity beliefs are exempt.
        let mut general_rels: Vec<f64> = cand
            .iter()
            .filter(|(_, b, _, _, _)| !is_exempt(b) && !is_identity(b))
            .map(|(_, _, _, _, rel)| *rel)
            .collect();
        if let Some(threshold) = gap_threshold(&mut general_rels, cfg.recall.gap_min) {
            cand.retain(|(_, b, _, _, rel)| is_exempt(b) || is_identity(b) || *rel >= threshold);
        }
        // Effective cut = lowest general relevance that survived (for the EWMA).
        let lowest = cand
            .iter()
            .filter(|(_, b, _, _, _)| !is_exempt(b) && !is_identity(b))
            .map(|(_, _, _, _, rel)| *rel)
            .fold(f64::INFINITY, f64::min);
        effective_cut = lowest.is_finite().then_some(lowest);
    }

    if let Some(r) = relevance {
        cand.retain(|(i, b, _, _, _)| {
            b.layer == crate::store::beliefs::Layer::Identity
                || r.by_index.get(i).is_none_or(|p| *p >= r.drop_below)
        });
        for (i, _, _, s, _) in cand.iter_mut() {
            if let Some(p) = r.by_index.get(i) {
                *s *= 0.3 + 0.7 * f64::from(*p);
            }
        }
    }

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
    let alive: Vec<(usize, &Belief, f64, f64, f64)> = cand
        .into_iter()
        .filter(|(_, _, w, _, _)| *w > 0.0)
        .collect();
    if alive.is_empty() {
        return (vec![], effective_cut);
    }

    let scores: Vec<f64> = alive.iter().map(|(_, _, _, s, _)| *s).collect();

    // Normalized once, reused for both the diffusion graph and the DPP
    // kernel (previously cloned+renormalized separately for each).
    let mut normed: Vec<Vec<f32>> = alive.iter().map(|(_, b, _, _, _)| b.embedding.clone()).collect();
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
        for (pos, (orig, _, _, _, _)) in alive.iter().enumerate() {
            position_of.insert(*orig, pos);
        }
        alive
            .iter()
            .map(|(orig, _, _, _, _)| {
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
    // Conditioned two-pass DPP: Pass 1 selects up to IDENTITY_RESERVED slots
    // from Identity + standing epistemic profile beliefs; Pass 2 selects from
    // the remainder excluding near-duplicates (`cos >= 0.85`) of reserved picks.
    const PASS2_DEDUP_COSINE: f64 = 0.85;
    let identity_positions: Vec<usize> = alive
        .iter()
        .enumerate()
        .filter(|(_, (_, b, _, _, _))| {
            b.layer == crate::store::beliefs::Layer::Identity || is_standing_profile(b)
        })
        .map(|(pos, _)| pos)
        .collect();

    let mut idx: Vec<usize> = if identity_positions.is_empty() {
        dpp_sample(&kernel, MAX_BELIEFS, cfg.dpp.epsilon)
    } else {
        let reserved = IDENTITY_RESERVED.min(identity_positions.len());
        let mut picked = dpp_sample_restricted(&kernel, &identity_positions, reserved, cfg.dpp.epsilon);
        // The general pool: everything not reserved, minus near-duplicates of
        // the reserved picks, and minus profile beliefs that only cleared the
        // gentler identity floor -- they may take a reserved slot, never a
        // topical one.
        let floor = cfg.recall.min_cosine;
        let rest: Vec<usize> = (0..alive.len())
            .filter(|&p| {
                let (_, b, _, _, rel) = &alive[p];
                let topical_enough = !has_query
                    || b.pinned
                    || !(b.layer == crate::store::beliefs::Layer::Identity || is_standing_profile(b))
                    || *rel >= floor;
                !picked.contains(&p)
                    && topical_enough
                    && !picked
                        .iter()
                        .any(|&q| ops::cosine(&normed[p], &normed[q]) >= PASS2_DEDUP_COSINE)
            })
            .collect();
        picked.extend(dpp_sample_restricted(
            &kernel,
            &rest,
            MAX_BELIEFS.saturating_sub(picked.len()),
            cfg.dpp.epsilon,
        ));
        picked
    };
    idx.truncate(MAX_BELIEFS);

    let selected = idx
        .into_iter()
        .filter_map(|k| alive.get(k))
        .map(|(_, b, w, _, _)| SelectedBelief {
            belief: (*b).clone(),
            effective_weight: *w,
        })
        .collect();
    (selected, effective_cut)
}

/// The per-query gap-cut threshold over centered relevances: sort descending,
/// find the largest gap between adjacent scores, and when it clears `gap_min`
/// return the lower edge of the top cluster (keep scores `>=` it). `None` when
/// there is no significant gap (a tight distribution is kept whole) or fewer
/// than two scores. Mirrors Memorabilia's `adaptive_cut` gap logic so the two
/// plugins self-tune the same way.
fn gap_threshold(rels: &mut [f64], gap_min: f64) -> Option<f64> {
    if rels.len() < 2 {
        return None;
    }
    rels.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let mut max_gap = 0.0f64;
    let mut threshold = None;
    for w in rels.windows(2) {
        let gap = w[0] - w[1];
        if gap > max_gap {
            max_gap = gap;
            threshold = Some(w[0]); // keep scores >= the lower edge of the top cluster
        }
    }
    if max_gap >= gap_min {
        threshold
    } else {
        None
    }
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

/// Domainless durable epistemic/identity beliefs that belong in the standing
/// profile even when the turn query uses different vocabulary.
/// Keeps domain-scoped `Identity` beliefs subject to `min_cosine_identity`.
fn is_standing_profile(b: &Belief) -> bool {
    if b.domain.is_some() {
        return false;
    }
    if b.layer == crate::store::beliefs::Layer::Identity {
        return true;
    }
    b.layer == crate::store::beliefs::Layer::Context
        && matches!(
            b.kind,
            crate::store::beliefs::BeliefKind::Preference
                | crate::store::beliefs::BeliefKind::Trait
        )
        && b.confidence >= 0.65
        && b.support_count >= 2
}

/// Truncation order when over the token budget, after trimming excess
/// bullets from `[Working assumptions about you]`: drop low-urgency sections
/// first and protect the contrary prompts (`[Worth challenging]`,
/// `[In tension]`) longest.
pub fn truncation_order() -> &'static [&'static str] {
    // `[Worth challenging]` and `[In tension]` quote the beliefs they are
    // about, so they still read correctly once the assumptions list is gone.
    &[
        "[Check yourself]",
        "[Where I'm unsure]",
        "[Worth testing this turn]",
        "[Fresh angle]",
        "[Working assumptions about you]",
        "[Worth challenging]",
        "[In tension]",
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
    s.chars().count().div_ceil(4)
}

/// Minimum bullet lines preserved inside `[Working assumptions about you]`
/// while trimming to preserve dialectical sections (header + 2 bullets = 3 lines).
const MIN_ASSUMPTION_LINES: usize = 3;

/// Enforce the recall token budget: first trim trailing bullets from
/// `[Working assumptions about you]` (keeping at least the header and top 2
/// bullets) so active antisycophancy sections survive; if still over budget,
/// drop whole sections in `truncation_order()` without truncating mid-line.
pub fn cap_to_token_budget(block: String) -> String {
    if estimate_tokens(&block) <= RECALL_MAX_TOKENS {
        return block;
    }
    let mut sections: Vec<String> = block.split("\n\n").map(str::to_string).collect();
    // Phase 1: trim trailing bullet lines from `[Working assumptions about you]`.
    if let Some(idx) = sections
        .iter()
        .position(|s| s.starts_with("[Working assumptions about you]"))
    {
        let mut lines: Vec<String> = sections[idx].lines().map(str::to_string).collect();
        while lines.len() > MIN_ASSUMPTION_LINES && estimate_tokens(&sections.join("\n\n")) > RECALL_MAX_TOKENS {
            lines.pop();
            sections[idx] = lines.join("\n");
        }
    }
    // Phase 2: drop whole sections by priority order.
    for header in truncation_order() {
        if estimate_tokens(&sections.join("\n\n")) <= RECALL_MAX_TOKENS {
            break;
        }
        sections.retain(|s| !s.starts_with(header));
    }
    let mut joined = sections.join("\n\n");
    // Last resort: hard-truncate at a char boundary rather than exceeding budget.
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
    reflection_of(selected)
}

/// The reflection sentence over `selected` as given (already sorted).
fn reflection_of(selected: &[SelectedBelief]) -> String {
    if selected.is_empty() {
        return String::new();
    }
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
    render_reflection_block_full(reflection, None, None, worth_testing, unsure, check)
}

/// [`render_reflection_block`] with `[In tension]` and `[Worth challenging]`
/// (bare sentences, no headers), ordered so active dialectical signals
/// (`tension`, `challenge`, `worth_testing`) are protected longest.
pub fn render_reflection_block_full(
    reflection: &str,
    tension: Option<String>,
    challenge: Option<String>,
    worth_testing: Option<String>,
    unsure: Option<String>,
    check: Option<String>,
) -> String {
    render_reflection_block_with_novelty(
        reflection,
        tension,
        challenge,
        None,
        worth_testing,
        unsure,
        check,
    )
}

/// [`render_reflection_block_full`] with an optional first-person `[Fresh angle]`
/// sentence (no bracketed labels).
pub fn render_reflection_block_with_novelty(
    reflection: &str,
    tension: Option<String>,
    challenge: Option<String>,
    fresh_angle: Option<String>,
    worth_testing: Option<String>,
    unsure: Option<String>,
    check: Option<String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !reflection.is_empty() {
        parts.push(reflection.to_string());
    }
    for part in [tension, challenge, fresh_angle, worth_testing, unsure, check]
        .into_iter()
        .flatten()
    {
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join("\n\n")
}

/// The whole thought-seed block within the token budget. Over budget, the
/// weakest beliefs leave the reflection first (keeping at least two), so the
/// dialectical sentences after it survive; only then do trailing paragraphs
/// go ([`cap_reflection_to_token_budget`]). Trims the beliefs themselves
/// rather than re-parsing rendered text, so a belief containing ". " or
/// "; " cannot break it.
#[allow(clippy::too_many_arguments)]
pub fn render_reflection_block_capped(
    selected: &mut [SelectedBelief],
    tension: Option<String>,
    challenge: Option<String>,
    fresh_angle: Option<String>,
    worth_testing: Option<String>,
    unsure: Option<String>,
    check: Option<String>,
) -> String {
    render_reflection(selected); // sorts into display order
    let mut n = selected.len();
    loop {
        let block = render_reflection_block_with_novelty(
            &reflection_of(&selected[..n]),
            tension.clone(),
            challenge.clone(),
            fresh_angle.clone(),
            worth_testing.clone(),
            unsure.clone(),
            check.clone(),
        );
        if n <= 2 || estimate_tokens(&block) <= RECALL_MAX_TOKENS {
            return cap_reflection_to_token_budget(block);
        }
        n -= 1;
    }
}

/// `cap_to_token_budget`'s equivalent for the header-less thought-seed
/// block: drops whole trailing paragraphs (least important last), never the
/// reflection, then hard-truncates as a last resort. Trimming beliefs out of
/// the reflection first is [`render_reflection_block_capped`]'s job.
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
            kind: crate::store::beliefs::BeliefKind::Context,
        }
    }

    #[test]
    fn a_judge_drops_irrelevant_beliefs_but_never_identity_ones() {
        let e = |i: usize| {
            let mut v = vec![0.0_f32; 4];
            v[i] = 1.0;
            v
        };
        let cands = vec![
            belief("keep", "likes rust", 0.8, true, Layer::Context, None, e(0)),
            belief("drop", "likes cats", 0.8, true, Layer::Context, None, e(1)),
            belief("who", "is a teacher", 0.8, true, Layer::Identity, None, e(2)),
        ];
        let mut by_index = std::collections::HashMap::new();
        by_index.insert(0, 0.9);
        by_index.insert(1, 0.01);
        by_index.insert(2, 0.01);
        let r = Relevance { by_index, drop_below: 0.08 };
        let sel = select_beliefs_relevant_with(&cands, &[], None, &[], Some(&r), &Config::default());
        let ids: Vec<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        assert!(ids.contains(&"keep"));
        assert!(ids.contains(&"who"));
        assert!(!ids.contains(&"drop"));
        // Without a judge, all three are eligible.
        assert_eq!(select_beliefs_relevant(&cands, &[], None, &[], &Config::default()).len(), 3);
    }

    #[test]
    fn preselect_ranks_by_the_blended_score() {
        let e = vec![1.0_f32, 0.0];
        let cands = vec![
            belief("low", "low", 0.2, true, Layer::Context, None, e.clone()),
            belief("high", "high", 0.9, true, Layer::Context, None, e),
        ];
        assert_eq!(preselect(&cands, &[], None, 1), vec![1]);
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
        // Inferred, not stated: a direct statement carries full weight
        // whether or not it has been tested (`Belief::full_weight`).
        let mut untested_high = belief("a", "untested high", 0.8, false, Layer::Context, Some("d"), e1);
        untested_high.provenance = Provenance::InferredPattern;
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

    #[test]
    fn centered_floor_drops_the_off_topic_belief() {
        // Query aligned to axis 0; an axis-1 belief is off topic. Raw cosine
        // can't separate them once they share a common direction, but after
        // mean-centering the aligned belief stays and the off-topic one — even
        // at higher standing weight — is dropped.
        let e0 = vec![1.0_f32, 0.0, 0.0];
        let e1 = vec![0.0_f32, 1.0, 0.0];
        let on = belief("on", "on-axis", 0.7, true, Layer::Context, Some("d"), e0.clone());
        let off = belief("off", "off-axis", 0.9, true, Layer::Context, Some("d"), e1);
        let sel = select_beliefs_relevant(&[on, off], &e0, None, &[], &Config::default());
        let ids: Vec<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        assert_eq!(ids, vec!["on"], "only the on-topic belief clears the floor");
    }

    #[test]
    fn centered_floor_is_a_noop_without_a_query() {
        // No query => no cosine => the gate must not drop anything (the
        // `select_beliefs` weight-only path is unchanged).
        let a = belief("a", "a", 0.7, true, Layer::Context, None, vec![1.0_f32, 0.0]);
        let b = belief("b", "b", 0.7, true, Layer::Context, None, vec![0.0_f32, 1.0]);
        assert_eq!(select_beliefs(&[a, b], None, &Config::default()).len(), 2);
    }

    #[test]
    fn centered_floor_exempts_pinned() {
        // A pinned off-axis belief survives the floor; a plain off-axis one at
        // the same relevance is dropped.
        let e0 = vec![1.0_f32, 0.0, 0.0];
        let e1 = vec![0.0_f32, 1.0, 0.0];
        let mut pinned = belief("pin", "pinned off", 0.7, true, Layer::Context, Some("d"), e1.clone());
        pinned.pinned = true;
        let ctx = belief("ctx", "plain off", 0.7, true, Layer::Context, Some("d"), e1);
        let on = belief("on", "on-axis", 0.7, true, Layer::Context, Some("d"), e0.clone());
        let sel = select_beliefs_relevant(&[pinned, ctx, on], &e0, None, &[], &Config::default());
        let ids: std::collections::HashSet<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        assert!(ids.contains("on"));
        assert!(ids.contains("pin"), "pinned is exempt from the floor");
        assert!(!ids.contains("ctx"), "a plain off-axis belief is dropped");
    }

    #[test]
    fn gap_threshold_keeps_top_cluster() {
        // Two tight high scores, a wide gap, then a weak tail.
        let mut rels = vec![0.02, 0.30, 0.05, 0.28];
        let t = gap_threshold(&mut rels, 0.08).expect("a significant gap exists");
        assert!((t - 0.28).abs() < 1e-12, "threshold is the lower edge of the top cluster");
    }

    #[test]
    fn gap_threshold_none_for_tight_distribution() {
        // All adjacent gaps below gap_min → keep everything (no cut).
        let mut rels = vec![0.30, 0.27, 0.25];
        assert!(gap_threshold(&mut rels, 0.08).is_none());
    }

    #[test]
    fn gap_threshold_none_for_single_score() {
        let mut rels = vec![0.30];
        assert!(gap_threshold(&mut rels, 0.08).is_none());
    }

    #[test]
    fn centered_gap_cut_drops_the_weak_tail() {
        // Query on axis 0. One belief squarely on-axis, one partly on-axis
        // (weak but positive relevance), one off-axis. After centering, the
        // on-axis belief leads; the partial one trails by a wide gap and is
        // cut; the off-axis one was already below the floor. Only the strong
        // match is injected.
        let e_on = vec![1.0_f32, 0.0, 0.0];
        let e_weak = vec![0.6_f32, 0.8, 0.0]; // shares the query axis, but mostly elsewhere
        let e_off = vec![0.0_f32, 1.0, 0.0];
        let strong = belief("strong", "strong", 0.7, true, Layer::Context, Some("d"), e_on.clone());
        let weak = belief("weak", "weak", 0.7, true, Layer::Context, Some("d"), e_weak);
        let off = belief("off", "off", 0.7, true, Layer::Context, Some("d"), e_off);
        let sel = select_beliefs_relevant(&[strong, weak, off], &e_on, None, &[], &Config::default());
        let ids: std::collections::HashSet<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        assert!(ids.contains("strong"), "the strong match is kept");
        assert!(!ids.contains("off"), "the off-axis belief is below the floor");
        // The weak partial match is dropped by the gap cut, not the floor.
        assert!(!ids.contains("weak"), "the weak tail is cut away from the strong cluster");
    }

    #[test]
    fn identity_uses_the_gentler_floor() {
        // With a strict general floor but a permissive identity floor, an
        // off-axis identity belief is kept while an identical-direction context
        // belief is dropped.
        let mut cfg = Config::default();
        cfg.recall.min_cosine = 0.5;
        cfg.recall.min_cosine_identity = -2.0; // below any cosine → identity exempt
        let e0 = vec![1.0_f32, 0.0, 0.0];
        let e1 = vec![0.0_f32, 1.0, 0.0];
        let id_off = belief("id", "identity off", 0.7, true, Layer::Identity, Some("d"), e1.clone());
        let ctx_off = belief("ctx", "context off", 0.7, true, Layer::Context, Some("d"), e1);
        let on = belief("on", "on-axis", 0.7, true, Layer::Context, Some("d"), e0.clone());
        let sel = select_beliefs_relevant(&[id_off, ctx_off, on], &e0, None, &[], &cfg);
        let ids: std::collections::HashSet<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        assert!(ids.contains("id"), "identity kept under its gentler floor");
        assert!(!ids.contains("ctx"), "context dropped under the strict floor");
    }

    #[test]
    fn cap_to_token_budget_trims_assumptions_before_dropping_challenge_or_tension() {
        let mut long_bullets = String::new();
        for i in 0..15 {
            if i > 0 {
                long_bullets.push('\n');
            }
            long_bullets.push_str(&format!(
                "- The user has recurring preference number {i} with a sufficiently descriptive detail sentence."
            ));
        }
        let block = crate::antisycophancy::render_block_full(
            &long_bullets,
            Some("Tension between A and B.".into()),
            Some("Challenge this stance.".into()),
            Some("Check whether they still want X.".into()),
            None,
            None,
        );
        let capped = cap_to_token_budget(block);
        assert!(estimate_tokens(&capped) <= RECALL_MAX_TOKENS);
        assert!(capped.contains("[In tension]"), "tension must survive bullet trimming: {capped}");
        assert!(capped.contains("[Worth challenging]"), "challenge must survive bullet trimming: {capped}");
        assert!(capped.contains("[Working assumptions about you]"));
    }

    fn standing(emb: Vec<f32>) -> Belief {
        let mut b = belief("stand", "Wants the recommendation first", 0.75, true, Layer::Context, None, emb);
        b.kind = crate::store::beliefs::BeliefKind::Preference;
        b.support_count = 2;
        b
    }

    #[test]
    fn standing_profile_needs_modest_relevance_and_keeps_to_reserved_slots() {
        let mut cfg = Config::default();
        cfg.recall.min_cosine = 0.5;
        let pool = |q: &[f32]| {
            let cands = [
                standing(vec![0.6, 0.8, 0.0]),
                belief("on", "on-axis", 0.7, true, Layer::Context, Some("d"), vec![1.0, 0.0, 0.0]),
                belief("z", "z-axis", 0.7, true, Layer::Context, Some("e"), vec![0.0, 0.0, 1.0]),
                belief("neg", "neg-y", 0.7, true, Layer::Context, Some("f"), vec![0.0, -1.0, 0.0]),
            ];
            select_beliefs_relevant(&cands, q, None, &[], &cfg)
                .into_iter()
                .map(|s| s.belief.id)
                .collect::<std::collections::HashSet<String>>()
        };
        // Mildly related (above the identity floor, below the topical one):
        // shown, through a reserved slot.
        assert!(pool(&[1.0, 0.0, 0.0]).contains("stand"));
        // Unrelated to the turn: not shown.
        assert!(!pool(&[0.0, 0.0, 1.0]).contains("stand"));
    }

    #[test]
    fn second_pass_drops_near_duplicates_of_reserved_picks() {
        let cfg = Config::default();
        let mut near_dup = belief(
            "dup",
            "Leads with the answer",
            0.7,
            true,
            Layer::Context,
            Some("writing"),
            vec![0.62, 0.78, 0.0],
        );
        near_dup.pinned = true; // past every floor, so only the dedup can drop it
        let other = belief("z", "z-axis", 0.7, true, Layer::Context, Some("e"), vec![0.0, 0.0, 1.0]);
        let sel = select_beliefs_relevant(
            &[standing(vec![0.6, 0.8, 0.0]), near_dup, other],
            &[0.6, 0.8, 0.0],
            None,
            &[],
            &cfg,
        );
        let ids: std::collections::HashSet<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
        assert!(ids.contains("stand"));
        assert!(!ids.contains("dup"), "Pass 2 must drop near-duplicates of reserved picks");
    }

    #[test]
    fn capped_reflection_trims_beliefs_even_when_their_text_has_sentence_breaks() {
        let mut selected: Vec<SelectedBelief> = (0..40)
            .map(|i| SelectedBelief {
                belief: belief(
                    &format!("b{i:02}"),
                    &format!("Belief {i}. It has a sentence break; and a semicolon in a fairly long description"),
                    0.7,
                    true,
                    Layer::Context,
                    None,
                    vec![1.0, 0.0],
                ),
                effective_weight: 1.0 - i as f64 / 100.0,
            })
            .collect();
        let block = render_reflection_block_capped(
            &mut selected,
            Some("Tension sentence.".into()),
            Some("Challenge sentence.".into()),
            None,
            None,
            None,
            None,
        );
        assert!(estimate_tokens(&block) <= RECALL_MAX_TOKENS);
        assert!(block.contains("Challenge sentence.") && block.contains("Tension sentence."));
        assert!(block.contains("Belief 0."), "the strongest belief is kept");
        assert!(!block.contains("Belief 39."), "the weakest go first");
    }
}
