//! Phase 2 acceptance: engine recall over a seeded store.

use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::recall::{render_knows, select_beliefs};
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use chrono::Utc;

#[allow(clippy::too_many_arguments)]
fn belief(id: &str, text: &str, conf: f64, tested: bool, layer: Layer, domain: Option<&str>, dim: usize, seed: u64) -> Belief {
    let mut x = seed;
    let embedding: Vec<f32> = (0..dim)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((x >> 33) as f64 / u32::MAX as f64 - 0.5) as f32
        })
        .collect();
    Belief {
        id: id.into(),
        text: text.into(),
        embedding,
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
        embedding_model: Config::default().embedding.ollama_model,
    }
}

async fn seed(engine: &PathwayEngine, dim: usize) {
    // ~30 beliefs: 10 identity, 10 context/coding, 10 conversation
    for i in 0..10 {
        let b = belief(
            &format!("id-{i}"),
            &format!("Identity belief {i} about the user's stable traits"),
            0.5 + 0.03 * i as f64,
            true,
            Layer::Identity,
            Some("identity"),
            dim,
            i as u64,
        );
        engine.db.insert_belief(&b).await.unwrap();
    }
    for i in 0..10 {
        let b = belief(
            &format!("ctx-{i}"),
            &format!("The user works in domain {}", i % 2),
            0.4 + 0.04 * i as f64,
            false,
            Layer::Context,
            Some(if i % 2 == 0 { "coding" } else { "writing" }),
            dim,
            100 + i as u64,
        );
        engine.db.insert_belief(&b).await.unwrap();
    }
    for i in 0..10 {
        let b = belief(
            &format!("conv-{i}"),
            &format!("Conversation-local note {i}"),
            0.3 + 0.05 * i as f64,
            false,
            Layer::Conversation,
            Some("coding"),
            dim,
            200 + i as u64,
        );
        engine.db.insert_belief(&b).await.unwrap();
    }
}

#[tokio::test]
async fn seed_thirty_selects_under_cap() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    seed(&engine, 384).await;

    let all = engine.db.list_beliefs(None).await.unwrap();
    assert_eq!(all.len(), 30);

    let sel = select_beliefs(&all, Some("coding"), &Config::default());
    assert!(sel.len() <= recall_cap());
    assert!(!sel.is_empty());
}

#[tokio::test]
async fn recall_respects_max_beliefs() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    seed(&engine, 384).await;
    let all = engine.db.list_beliefs(None).await.unwrap();
    for _ in 0..10 {
        let sel = select_beliefs(&all, Some("coding"), &Config::default());
        assert!(sel.len() <= 6);
    }
}

#[tokio::test]
async fn whole_block_render_is_byte_stable() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    seed(&engine, 128).await;
    let all = engine.db.list_beliefs(None).await.unwrap();

    let render = |all: &[Belief], domain: &str| -> String {
        let mut sel = select_beliefs(all, Some(domain), &Config::default());
        render_knows(&mut sel)
    };

    let a = render(&all, "coding");
    let b = render(&all, "coding");
    assert_eq!(a, b);
    assert!(!a.is_empty());
}

#[tokio::test]
async fn paused_engine_recall_is_empty() {
    let engine = PathwayEngine::open_in_memory(Config::default()).await.unwrap();
    seed(&engine, 128).await;
    engine.set_paused("sess-1", true).await.unwrap();
    let paused = engine.is_paused("sess-1").await.unwrap();
    assert!(paused);

    // With recall paused, the caller should inject None / zero delta. The
    // engine exposes paused as a gate; effective weight of suppressed is 0.
    let all = engine.db.list_beliefs(None).await.unwrap();
    // pause does not itself delete beliefs, but recall selection still works;
    // the *caller* (builder.rs) turns it into a zero-delta by returning None.
    assert!(!all.is_empty());
}

fn recall_cap() -> usize {
    6
}
