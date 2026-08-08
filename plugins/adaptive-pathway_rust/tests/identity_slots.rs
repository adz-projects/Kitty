//! Reserved identity slots (`recall::IDENTITY_RESERVED`).
//!
//! `Layer::Identity` beliefs have a 365-day half-life and are only reachable
//! by promotion through `consolidate.rs`'s gates, but they compete for one of
//! `MAX_BELIEFS` slots on raw effective weight like everything else -- so a
//! burst of fresh, highly-relevant conversation beliefs could evict all of
//! them from a turn. These tests pin that they can't.

use adaptive_pathway::config::Config;
use adaptive_pathway::recall::{select_beliefs, IDENTITY_RESERVED, MAX_BELIEFS};
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use chrono::Utc;

/// `confidence` drives `effective_weight`, so it's the lever for making a
/// belief win or lose a contested slot.
fn belief(id: &str, layer: Layer, confidence: f64, emb: Vec<f32>) -> Belief {
    let now = Utc::now();
    Belief {
        id: id.into(),
        text: format!("belief {id}"),
        embedding: emb,
        confidence,
        provenance: Provenance::DirectStatement,
        layer,
        tested: true,
        domain: None,
        tier: "context".into(),
        support_count: 3,
        distinct_sessions: 2,
        contradict_count: 0,
        pinned: false,
        last_confirmed_at: Some(now),
        consolidated_at: None,
        created_at: now,
        updated_at: now,
        session_id: None,
        embedding_model: Config::default().embedding.ollama_model.clone(),
    }
}

/// Mutually near-orthogonal unit vectors, so DPP's diversity term treats every
/// candidate as equally novel and selection is decided purely by weight --
/// which is what makes "the weak identity belief would have lost" a true
/// statement rather than an artifact of the kernel.
fn axis(i: usize, n: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; n];
    v[i] = 1.0;
    v
}

#[test]
fn weak_identity_beliefs_keep_their_slots_against_a_strong_field() {
    let n = 10;
    let mut cands = vec![
        belief("id-1", Layer::Identity, 0.05, axis(0, n)),
        belief("id-2", Layer::Identity, 0.05, axis(1, n)),
    ];
    // Eight much stronger context beliefs -- enough to fill every slot on
    // weight alone, several times over.
    for i in 2..n {
        cands.push(belief(&format!("ctx-{i}"), Layer::Context, 0.95, axis(i, n)));
    }

    let selected = select_beliefs(&cands, None, &Config::default());
    let identity_picks = selected
        .iter()
        .filter(|s| s.belief.layer == Layer::Identity)
        .count();

    assert_eq!(
        identity_picks, IDENTITY_RESERVED,
        "both identity beliefs must hold their reserved slots despite losing on weight"
    );
    assert_eq!(selected.len(), MAX_BELIEFS, "the rest of the block still fills up");
}

#[test]
fn identity_beliefs_do_not_get_extra_slots_beyond_the_reservation() {
    // The reservation is a floor, not a quota: identity beliefs beyond
    // IDENTITY_RESERVED compete normally, and weak ones should lose.
    let n = 10;
    let mut cands = Vec::new();
    for i in 0..5 {
        cands.push(belief(&format!("id-{i}"), Layer::Identity, 0.05, axis(i, n)));
    }
    for i in 5..n {
        cands.push(belief(&format!("ctx-{i}"), Layer::Context, 0.95, axis(i, n)));
    }

    let selected = select_beliefs(&cands, None, &Config::default());
    let identity_picks = selected
        .iter()
        .filter(|s| s.belief.layer == Layer::Identity)
        .count();

    assert_eq!(
        identity_picks, IDENTITY_RESERVED,
        "weak identity beliefs past the reservation must still lose on merit"
    );
}

#[test]
fn no_identity_beliefs_means_no_wasted_slots() {
    let n = 10;
    let cands: Vec<Belief> = (0..n)
        .map(|i| belief(&format!("ctx-{i}"), Layer::Context, 0.8, axis(i, n)))
        .collect();

    let selected = select_beliefs(&cands, None, &Config::default());
    assert_eq!(
        selected.len(),
        MAX_BELIEFS,
        "with no identity candidates the reserved pass must select nothing and \
         the general pass must take every slot"
    );
}

#[test]
fn never_exceeds_the_slot_budget() {
    let n = 20;
    let mut cands = Vec::new();
    for i in 0..6 {
        cands.push(belief(&format!("id-{i}"), Layer::Identity, 0.9, axis(i, n)));
    }
    for i in 6..n {
        cands.push(belief(&format!("ctx-{i}"), Layer::Context, 0.9, axis(i, n)));
    }

    let selected = select_beliefs(&cands, None, &Config::default());
    assert!(selected.len() <= MAX_BELIEFS);
    // No belief may be selected twice by the two passes.
    let mut ids: Vec<&str> = selected.iter().map(|s| s.belief.id.as_str()).collect();
    ids.sort_unstable();
    let unique = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), unique, "the reserved and general passes must not double-pick");
}

#[test]
fn selection_stays_deterministic_with_reserved_slots() {
    let n = 10;
    let mut cands = vec![
        belief("id-1", Layer::Identity, 0.3, axis(0, n)),
        belief("id-2", Layer::Identity, 0.3, axis(1, n)),
    ];
    for i in 2..n {
        cands.push(belief(&format!("ctx-{i}"), Layer::Context, 0.6, axis(i, n)));
    }

    let a: Vec<String> =
        select_beliefs(&cands, None, &Config::default()).iter().map(|s| s.belief.id.clone()).collect();
    let b: Vec<String> =
        select_beliefs(&cands, None, &Config::default()).iter().map(|s| s.belief.id.clone()).collect();
    assert_eq!(a, b);
}
