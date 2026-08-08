//! End-to-end coverage for `PathwayEngine::recall` -- issues #7-11: the
//! anti-sycophancy sections, domain routing, assumption surfacing, and
//! suppression filtering all wired together through the actual entry point
//! `bigtiny_rust`'s agent loop calls.

use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::store::assumptions::{Assumption, AssumptionState};
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use adaptive_pathway::store::suppressions::SuppressReason;
use chrono::Utc;

fn belief(id: &str, text: &str, domain: Option<&str>, emb: Vec<f32>) -> Belief {
    let now = Utc::now();
    Belief {
        id: id.into(),
        text: text.into(),
        embedding: emb,
        confidence: 0.7,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
        tested: true,
        domain: domain.map(|s| s.into()),
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
        embedding_model: Config::default().embedding.ollama_model,
    }
}

#[tokio::test]
async fn no_beliefs_recalls_nothing() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    assert_eq!(engine.recall("s1", "hello").await, None);
}

#[tokio::test]
async fn paused_session_recalls_nothing_even_with_beliefs() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user likes concise answers.", None, vec![1.0, 0.0]))
        .await
        .unwrap();
    engine.set_paused("s1", true).await.unwrap();
    assert_eq!(engine.recall("s1", "hello").await, None);
}

#[tokio::test]
async fn basic_recall_renders_the_knows_section_and_footer() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user prefers terse code comments.", None, vec![1.0, 0.0]))
        .await
        .unwrap();

    let block = engine.recall("s1", "").await.expect("beliefs exist, must recall something");
    assert!(block.contains("[Working assumptions about you]"));
    assert!(block.contains("terse code comments"));
    assert!(block.contains(adaptive_pathway::recall::FOOTER));
}

#[tokio::test]
async fn suppressed_beliefs_never_surface_in_recall() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    let text = "The user hates dark mode.";
    engine.db.insert_belief(&belief("b1", text, None, vec![1.0, 0.0])).await.unwrap();

    // Directly resolvable by id -- forget it as "wrong".
    let dropped = engine
        .db
        .forget_belief_by_id("b1", SuppressReason::Wrong)
        .await
        .unwrap();
    assert!(dropped.is_some());

    // The belief row survives a "wrong" forget (only "private" hard-deletes)
    // but must never surface in recall again.
    assert!(engine.db.get_belief("b1").await.unwrap().is_some());
    let block = engine.recall("s1", "").await;
    assert!(
        block.is_none() || !block.unwrap().contains("dark mode"),
        "a suppressed belief must never appear in a recall block"
    );
}

#[tokio::test]
async fn domain_routing_prefers_in_domain_beliefs() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    // Two orthogonal-embedding beliefs in different domains. Querying along
    // the coding axis should surface the coding belief, not force the
    // cooking one out (cross-domain is downweighted, not excluded), but the
    // in-domain one should rank first.
    engine
        .db
        .insert_belief(&belief("coding", "The user writes Rust.", Some("coding"), vec![1.0, 0.0, 0.0]))
        .await
        .unwrap();
    engine
        .db
        .insert_belief(&belief("cooking", "The user bakes bread.", Some("cooking"), vec![0.0, 1.0, 0.0]))
        .await
        .unwrap();

    // The user_message embeds (via the hashing fallback, no live Ollama in
    // tests) to *some* vector -- we can't control its exact direction here,
    // so this test only asserts the mechanism doesn't crash and produces a
    // sensible block; the pure `infer_query_domain`/`domain_match` unit
    // tests in `domains.rs` cover the actual routing math precisely.
    let block = engine.recall("s1", "tell me about my Rust code").await;
    assert!(block.is_some());
}

#[tokio::test]
async fn worth_testing_surfaces_a_scheduled_and_surfaced_assumption() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user wants short replies.", None, vec![1.0, 0.0]))
        .await
        .unwrap();
    engine
        .db
        .insert_assumption(&Assumption {
            id: "a1".into(),
            belief_id: Some("b1".into()),
            text: "the user wants short replies".into(),
            confidence: 0.6,
            state: AssumptionState::Surfaced,
            flagged_at_exchange: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let block = engine.recall("s1", "").await.unwrap();
    assert!(block.contains("[Worth testing this turn]"));
    assert!(block.contains("the user wants short replies"));
}

#[tokio::test]
async fn scheduled_but_not_yet_surfaced_assumptions_do_not_render() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user wants short replies.", None, vec![1.0, 0.0]))
        .await
        .unwrap();
    engine
        .db
        .insert_assumption(&Assumption {
            id: "a1".into(),
            belief_id: Some("b1".into()),
            text: "the user wants short replies".into(),
            confidence: 0.6,
            state: AssumptionState::Scheduled, // not yet Surfaced
            flagged_at_exchange: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let block = engine.recall("s1", "").await.unwrap();
    assert!(!block.contains("[Worth testing this turn]"));
}

#[tokio::test]
async fn unsure_line_only_renders_on_the_twelve_exchange_cadence() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    // More beliefs than MAX_BELIEFS (6) so at least one is excluded from
    // [Working assumptions about you] and remains available for the uncertainty
    // pick -- with too few candidates, DPP selects all of them and nothing
    // is left over to ever be "unsure" about.
    for i in 0..10 {
        let angle = i as f32;
        engine
            .db
            .insert_belief(&belief(
                &format!("b{i}"),
                &format!("The user has preference number {i}."),
                None,
                vec![angle.sin(), angle.cos()],
            ))
            .await
            .unwrap();
    }

    // exchange_count starts at 0 -- not due.
    let block = engine.recall("s1", "").await.unwrap();
    assert!(!block.contains("[Where I'm unsure]"));

    // Bump to exactly 12.
    for _ in 0..12 {
        engine.db.bump_exchange("s1").await.unwrap();
    }
    let block = engine.recall("s1", "").await.unwrap();
    assert!(block.contains("[Where I'm unsure]"));
}

#[tokio::test]
async fn thought_seed_recalls_nothing_without_beliefs() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    assert_eq!(engine.recall_thought_seed("s1", "hello").await, None);
}

#[tokio::test]
async fn thought_seed_is_none_when_paused_even_with_beliefs() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user likes concise answers.", None, vec![1.0, 0.0]))
        .await
        .unwrap();
    engine.set_paused("s1", true).await.unwrap();
    assert_eq!(engine.recall_thought_seed("s1", "hello").await, None);
}

#[tokio::test]
async fn thought_seed_renders_as_reflection_not_a_labeled_block() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user prefers terse code comments.", None, vec![1.0, 0.0]))
        .await
        .unwrap();

    let seed = engine.recall_thought_seed("s1", "").await.expect("beliefs exist, must seed something");
    assert!(seed.contains("terse code comments"));
    // Must NOT carry the system-block framing -- no section header, no footer.
    assert!(!seed.contains("[Working assumptions about you]"));
    assert!(!seed.contains(adaptive_pathway::recall::FOOTER));
}

#[tokio::test]
async fn recall_stays_under_the_token_budget_with_many_beliefs() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    for i in 0..40 {
        let text = format!(
            "The user has a fairly detailed and specific preference number {i} that takes up a meaningful amount of space in the rendered block."
        );
        engine
            .db
            .insert_belief(&belief(&format!("b{i}"), &text, None, vec![(i as f32).sin(), (i as f32).cos()]))
            .await
            .unwrap();
    }
    let block = engine.recall("s1", "").await.unwrap();
    let approx_tokens = (block.chars().count() + 3) / 4;
    assert!(
        approx_tokens <= adaptive_pathway::recall::RECALL_MAX_TOKENS,
        "recall block ({approx_tokens} est. tokens) exceeded the {}-token budget",
        adaptive_pathway::recall::RECALL_MAX_TOKENS
    );
}

// --- Dialectical framing (Workstream A) -------------------------------------
//
// The point of these is not to pin exact prose -- the wording is expected to
// be tuned -- but to pin the *stance*: recalled beliefs are a provisional
// prior to test the request against, never a profile to conform to. The
// original seed template ended "I'll let that inform my tone without stating
// it outright", which is a conformity instruction, and the seed path rendered
// only the fact list while dropping every anti-sycophancy signal. Both are
// regressions worth failing a build over.

#[tokio::test]
async fn neither_render_path_tells_the_model_to_conform() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user prefers terse code comments.", None, vec![1.0, 0.0]))
        .await
        .unwrap();

    let block = engine.recall("s1", "").await.unwrap();
    let seed = engine.recall_thought_seed("s1", "").await.unwrap();

    for (label, text) in [("system block", &block), ("thought seed", &seed)] {
        let lower = text.to_lowercase();
        assert!(
            !lower.contains("inform my tone"),
            "{label} must not instruct the model to shape itself to the profile"
        );
        assert!(
            !lower.contains("without stating it outright"),
            "{label} must not instruct the model to apply the profile covertly"
        );
        // The dialectical instruction: check the prior against the request.
        assert!(
            lower.contains("stale") && lower.contains("check"),
            "{label} must frame recalled beliefs as a checkable, possibly-stale prior"
        );
    }
}

#[tokio::test]
async fn the_thought_seed_carries_the_same_signals_as_the_system_block() {
    // Regression guard: `recall_thought_seed` used to render only the fact
    // list, silently dropping [Worth testing this turn] / [Where I'm unsure] /
    // [Check yourself] -- i.e. it kept the profile and threw away the
    // machinery that makes the profile something to question, leaving the
    // seeded path strictly more sycophantic than the block it replaces.
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    engine
        .db
        .insert_belief(&belief("b1", "The user wants short replies.", None, vec![1.0, 0.0]))
        .await
        .unwrap();
    engine
        .db
        .insert_assumption(&Assumption {
            id: "a1".into(),
            belief_id: Some("b1".into()),
            text: "the user wants short replies".into(),
            confidence: 0.6,
            state: AssumptionState::Surfaced,
            flagged_at_exchange: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();

    let block = engine.recall("s1", "").await.unwrap();
    let seed = engine.recall_thought_seed("s1", "").await.unwrap();

    // Same underlying signal, two framings: the block labels it, the seed
    // carries it bare so a `<think>` turn doesn't read as injected scaffolding.
    assert!(block.contains("[Worth testing this turn]"));
    assert!(!seed.contains("[Worth testing this turn]"));
    for text in [&block, &seed] {
        assert!(
            text.contains("the user wants short replies"),
            "both paths must surface the live assumption, only the framing differs"
        );
    }
}

#[test]
fn the_reflection_cap_drops_whole_trailing_sections_before_truncating() {
    use adaptive_pathway::recall::{cap_reflection_to_token_budget, RECALL_MAX_TOKENS};

    let facts = "a".repeat(RECALL_MAX_TOKENS * 4);
    let block = format!("{facts}\n\nsecond section\n\nthird section");
    let capped = cap_reflection_to_token_budget(block);

    // Least-important-last ordering means the trailing sections go first...
    assert!(!capped.contains("third section"));
    assert!(!capped.contains("second section"));
    // ...and the hard char truncate is only the last resort for a single
    // oversized section, mirroring `cap_to_token_budget`'s behaviour.
    assert!(capped.chars().count() <= RECALL_MAX_TOKENS * 4);
}
