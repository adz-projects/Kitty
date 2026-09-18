//! The engine's soft no-op (claude.md, core principle 9): when the engine
//! has nothing to say, the payload is byte-identical to no-memory behavior.
//! Pinned from Phase 0 so every phase that adds recall behavior must keep
//! it.

use std::sync::Arc;

use memorabilia::config::Config;
use memorabilia::embed::HashEmbedder;
use memorabilia::engine::Engine;
use memorabilia::store::Db;
use memorabilia::traits::{MemoryVectorIndex, MockChat};

fn empty_engine() -> Engine {
    let runtime = tokio::runtime::Handle::current();
    let db = runtime.block_on(Db::open_in_memory()).unwrap();
    let cfg = Config::default();
    Engine::new(
        cfg.clone(),
        db,
        Arc::new(MockChat {
            response: serde_json::json!({}),
        }),
        Arc::new(HashEmbedder::new(cfg.embedding_dim)),
        Arc::new(MemoryVectorIndex::new()),
    )
}

#[tokio::test]
async fn empty_engine_recall_is_a_stable_byte_identical_noop() {
    let dim = Config::default().embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    let engine = Engine::new(
        Config::default(),
        db,
        Arc::new(MockChat {
            response: serde_json::json!({}),
        }),
        Arc::new(HashEmbedder::new(dim)),
        Arc::new(MemoryVectorIndex::new()),
    );

    // Identical state → identical (empty) payload, every call.
    let mut payloads = Vec::new();
    for _ in 0..5 {
        payloads.push(engine.recall("anything at all").await);
    }
    assert!(payloads.iter().all(|p| p.is_none()));

    // Two independently constructed empty engines agree byte-for-byte.
    let other = Engine::new(
        Config::default(),
        Db::open_in_memory().await.unwrap(),
        Arc::new(MockChat {
            response: serde_json::json!({}),
        }),
        Arc::new(HashEmbedder::new(dim)),
        Arc::new(MemoryVectorIndex::new()),
    );
    assert_eq!(
        engine.recall("x").await.as_ref(),
        other.recall("x").await.as_ref()
    );
}

// `empty_engine` exists so future test modules can build the baseline state
// in both sync and async contexts.
#[allow(dead_code)]
fn _ensure_helper_compiles() {
    let _ = Box::new(empty_engine);
}
