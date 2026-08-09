//! Phase 2b gate: adaptive-pathway recall must still work once embeddings are
//! served in-process by the local engine instead of over HTTP to Ollama.
//!
//! ANDROID.md makes this the *ordering* rule for Phase 2b — verify recall on
//! the new embedder before deleting the old one — so it gets a real test
//! against real weights rather than a mock.
//!
//! Opt-in: set `KITTY_TEST_EMBED_GGUF` to a Qwen3-Embedding-style GGUF. Skips
//! with a message otherwise, matching `local::provider`'s convention.

#![cfg(feature = "local-engine")]

use adaptive_pathway::config::Config;
use adaptive_pathway::engine::PathwayEngine;
use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};
use bigtiny_rust::config::LocalEngineConfig;
use bigtiny_rust::local::{pathway_embed, LocalPathwayEmbedder, SlotManager};
use chrono::Utc;

fn belief(id: &str, text: &str, embedding: Vec<f32>, model: &str) -> Belief {
    Belief {
        id: id.into(),
        text: text.into(),
        embedding,
        confidence: 0.8,
        provenance: Provenance::DirectStatement,
        layer: Layer::Context,
        tested: true,
        domain: None,
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
        embedding_model: model.into(),
    }
}

#[tokio::test]
async fn recall_works_on_the_in_process_embedder() {
    let Ok(gguf) = std::env::var("KITTY_TEST_EMBED_GGUF") else {
        eprintln!("skipping: set KITTY_TEST_EMBED_GGUF to an embedding GGUF to run");
        return;
    };

    let local_cfg = LocalEngineConfig {
        enabled: true,
        embed_model_path: gguf.clone(),
        ..Default::default()
    };
    let slots = SlotManager::new();
    let embedder = pathway_embed::embedder_for(&slots, &local_cfg)
        .expect("a configured, enabled engine must yield an embedder");

    let space = LocalPathwayEmbedder::space_tag(&gguf);
    let mut ap_cfg = Config::default();
    ap_cfg.embedding.ollama_model = space.clone();

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("pathway.db");
    let engine = PathwayEngine::open_with_embedder(
        &db_path.to_string_lossy(),
        ap_cfg,
        Some(embedder),
    )
    .await
    .expect("pathway engine opens");

    // Embed through the engine's own provider, so the stored vectors are in
    // exactly the space recall will query with.
    let (cooking, semantic) = engine
        .embed
        .embed_with_space("The user prefers to cook dinner from scratch on weeknights")
        .await;
    assert!(
        semantic,
        "the in-process embedder must produce the semantic space, not the hash fallback — \
         if this fails the model did not load and everything below is meaningless"
    );
    let (rust, _) = engine
        .embed
        .embed_with_space("The user writes Rust and dislikes unnecessary abstraction")
        .await;

    engine
        .db
        .insert_belief(&belief("b-cooking", "The user prefers to cook dinner from scratch on weeknights", cooking, &space))
        .await
        .unwrap();
    engine
        .db
        .insert_belief(&belief("b-rust", "The user writes Rust and dislikes unnecessary abstraction", rust, &space))
        .await
        .unwrap();

    // The space tag is a filter, not a label: a mismatch here returns zero
    // candidates and recall silently goes quiet.
    let candidates = engine
        .db
        .list_recall_candidates("s1", &space)
        .await
        .expect("candidate query");
    assert_eq!(
        candidates.len(),
        2,
        "beliefs tagged {space} must be visible to recall; got {}",
        candidates.len()
    );

    let block = engine
        .recall("s1", "what should I make for dinner tonight?")
        .await
        .expect("recall must produce a block when relevant beliefs exist");
    assert!(
        block.contains("cook dinner from scratch"),
        "the food question should surface the cooking belief; got:\n{block}"
    );
}

/// A belief embedded by a *previous* model must not be silently compared
/// against in-process vectors — it has to fall out of the candidate set so
/// `reembed_stale_beliefs` can migrate it. This is the failure mode the space
/// tag exists to prevent, and it needs no GGUF to check.
#[tokio::test]
async fn beliefs_from_a_previous_embedding_space_are_excluded() {
    let space = LocalPathwayEmbedder::space_tag("/models/Qwen3-Embedding-0.6B-q4_k_m.gguf");
    let mut ap_cfg = Config::default();
    ap_cfg.embedding.ollama_model = space.clone();

    let engine = PathwayEngine::open_in_memory(ap_cfg).await.unwrap();
    engine
        .db
        .insert_belief(&belief(
            "old",
            "Embedded by the Ollama-era model",
            vec![0.1; Config::default().embedding_dim],
            "qwen3-embedding:0.6b",
        ))
        .await
        .unwrap();

    let candidates = engine.db.list_recall_candidates("s1", &space).await.unwrap();
    assert!(
        candidates.is_empty(),
        "a stale-space belief must be excluded from recall, not compared across spaces"
    );
}
