//! End-to-end coverage for spreading activation (`vector::spread`) wired
//! into `recall::select_beliefs`/`select_beliefs_relevant`: a weak belief
//! connected to a strong one via embedding similarity should be pulled into
//! the selected set when diffusion is enabled, and stay excluded when it
//! isn't -- the whole point of "ambient context" over isolated-fact
//! matching. DPP's existing near-duplicate-diversification guarantee must
//! still hold on top.

use adaptive_pathway::config::{Config, DiffusionConfig};
use adaptive_pathway::recall::select_beliefs;
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use chrono::Utc;

fn belief(id: &str, text: &str, confidence: f64, tested: bool, emb: Vec<f32>) -> Belief {
    let now = Utc::now();
    Belief {
        id: id.into(),
        text: text.into(),
        embedding: emb,
        confidence,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
        tested,
        domain: Some("d".into()),
        tier: "context".into(),
        support_count: 1,
        distinct_sessions: 1,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: Some(now),
        consolidated_at: None,
        embedding_model: Config::default().embedding.ollama_model,
        created_at: now,
        updated_at: now,
        session_id: None,
    }
}

fn diffusion_disabled() -> Config {
    let mut cfg = Config::default();
    cfg.diffusion = DiffusionConfig {
        enabled: false,
        ..cfg.diffusion
    };
    cfg
}

/// "A" is a strong, well-established, unrelated-to-everything-else belief --
/// it always wins the first DPP slot regardless of diffusion, and stays
/// uninvolved in the rest of this fixture (orthogonal to all).
///
/// "F0".."F5" and "B" are all equally weak (same low confidence/untested
/// weight) and compete for the remaining 5 slots. Critically, "B" is
/// embedded at a *moderate* cosine similarity to "F0" (0.6, just above the
/// diffusion edge threshold) -- not to "A". This matters: a candidate
/// boosted by diffusion from something that then itself gets DPP-picked
/// would face DPP's own redundancy downdate (which scales with similarity
/// squared) working *against* the boost, canceling it out for any
/// similarity high enough to cross the edge threshold in the first place.
/// Wiring B's connection through F0 -- one of many equally-weak, tied
/// candidates -- instead lets a *small* diffusion-driven edge decide which
/// of the tied pair (B or F0) gets picked *first*; whichever wins takes the
/// slot, and the other then eats the redundancy downdate instead of B.
/// F1..F5 are mutually orthogonal fillers with no diffusion edges to
/// anything, giving a clean tied baseline B/F0 must out-compete.
fn candidates() -> Vec<Belief> {
    let mut cands = vec![belief(
        "A",
        "The user is a backend engineer.",
        0.9,
        true,
        vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    )];
    cands.push(belief(
        "F0",
        "Unrelated filler 0.",
        0.15,
        false,
        vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    ));
    for (i, dim) in (3..8).enumerate() {
        let mut e = vec![0.0_f32; 8];
        e[dim] = 1.0;
        cands.push(belief(&format!("F{}", i + 1), &format!("Unrelated filler {}.", i + 1), 0.15, false, e));
    }
    cands.push(belief(
        "B",
        "The user cares about clean error handling.",
        0.15,
        false,
        // cosine(F0, B) = 0.6 (>= the 0.55 edge threshold); orthogonal to A
        // and to every F1..F5 (dims 3..7 untouched).
        vec![0.0, 0.6, 0.8, 0.0, 0.0, 0.0, 0.0, 0.0],
    ));
    cands
}

#[test]
fn diffusion_disabled_leaves_the_weak_related_belief_out() {
    let sel = select_beliefs(&candidates(), Some("d"), &diffusion_disabled());
    let ids: Vec<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
    assert!(
        !ids.contains(&"B"),
        "without diffusion, B ties F0 on raw score and loses the index tie-break: {ids:?}"
    );
    assert!(ids.contains(&"F0"), "F0 should win the tie instead: {ids:?}");
}

#[test]
fn diffusion_enabled_pulls_the_weak_related_belief_in() {
    let sel = select_beliefs(&candidates(), Some("d"), &Config::default());
    let ids: Vec<&str> = sel.iter().map(|s| s.belief.id.as_str()).collect();
    assert!(
        ids.contains(&"B"),
        "diffused activation from F0 should give B just enough edge to win the tie: {ids:?}"
    );
    assert!(
        !ids.contains(&"F0"),
        "F0 should lose its slot to B, then eat DPP's redundancy downdate against the now-picked B: {ids:?}"
    );
    // The strong, unrelated anchor must still be selected regardless.
    assert!(ids.contains(&"A"));
}

#[test]
fn diffusion_does_not_disturb_dpp_diversity_over_near_duplicates() {
    // Three equal-weight beliefs: two are near-identical embeddings, one is
    // clearly opposed. DPP must still surface the opposed one rather than
    // both near-duplicates, exactly as without diffusion.
    let base = vec![1.0_f32, 0.0];
    let similar = vec![0.99_f32, 0.10];
    let opposed = vec![-1.0_f32, 0.0];
    let cands = vec![
        belief("a", "technical", 0.6, true, base),
        belief("b", "technical-similar", 0.6, true, similar),
        belief("c", "technical-opposed", 0.6, true, opposed),
    ];
    let sel = select_beliefs(&cands, Some("d"), &Config::default());
    assert!(sel.iter().any(|s| s.belief.id == "c"), "the opposed belief must still surface");
}

#[test]
fn diffusion_selection_is_deterministic_across_repeat_calls() {
    let cfg = Config::default();
    let s1 = select_beliefs(&candidates(), Some("d"), &cfg);
    let s2 = select_beliefs(&candidates(), Some("d"), &cfg);
    let ids1: Vec<&str> = s1.iter().map(|s| s.belief.id.as_str()).collect();
    let ids2: Vec<&str> = s2.iter().map(|s| s.belief.id.as_str()).collect();
    assert_eq!(ids1, ids2);
}
