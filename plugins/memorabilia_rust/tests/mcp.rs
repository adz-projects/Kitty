//! The MCP lookup surface (BigTiny-plugin conversion): the on-demand read
//! path that complements context injection. Pinned here (behavior, not
//! wording): the two load-bearing tool names; a `memorabilia_search`
//! item_id resolves through `memorabilia_read_item`; the uniform
//! success/error envelope; an unknown id returns a structured error, not a
//! panic; an empty query is a soft success.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use serde_json::{json, Value};

use memorabilia::config::Config;
use memorabilia::embed::HashEmbedder;
use memorabilia::engine::Engine;
use memorabilia::learn::IngestInput;
use memorabilia::mcp::{MemorabiliaServer, ReadItemRequest, SearchRequest};
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::traits::MockChat;

const T0: &str = "2026-08-01T10:00:00Z";

async fn server_with_memory() -> MemorabiliaServer {
    let cfg = Config::default();
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    let response = json!({
        "propositions": [{
            "claim": "A relevant fact about the topic",
            "importance": "medium",
            "urgency": "unknown",
            "decay_class": "static",
            "urgency_expires_at": ""
        }],
        "disputes": []
    });
    let engine = Engine::new(
        cfg,
        db,
        Arc::new(MockChat { response }),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    );
    engine
        .ingest(&IngestInput {
            content: "databases replication shared topic tokens payload".into(),
            source_type: "Scraped".into(),
            source_name: "alpha.example".into(),
            source_entity: "alpha.example".into(),
            captured_at: T0.into(),
            intent: None,
        })
        .await;
    engine.drain_extraction(T0).await;
    MemorabiliaServer::new(Arc::new(engine))
}

#[tokio::test]
async fn advertises_exactly_the_two_lookup_tools() {
    let server = server_with_memory().await;
    assert_eq!(
        server.tool_names(),
        vec![
            "memorabilia_read_item".to_string(),
            "memorabilia_search".to_string()
        ]
    );
}

#[tokio::test]
async fn search_item_id_resolves_through_read_item() {
    let server = server_with_memory().await;

    let raw = server
        .memorabilia_search(Parameters(SearchRequest {
            query: "databases replication topic".into(),
            offset: None,
            limit: None,
        }))
        .await;
    let env: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(env["status"], "success");
    let items = env["data"].as_array().unwrap();
    assert!(!items.is_empty(), "search must surface the extracted item");
    let item_id = items[0]["item_id"].as_str().unwrap().to_string();
    assert!(items[0]["citation"].as_str().unwrap().contains("alpha.example"));

    let raw = server
        .memorabilia_read_item(Parameters(ReadItemRequest {
            item_id: item_id.clone(),
            offset: None,
            limit: None,
        }))
        .await;
    let env: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(env["status"], "success");
    assert_eq!(env["data"]["item_id"], item_id);
    assert_eq!(env["data"]["claim"], "A relevant fact about the topic");
    let chunks = env["data"]["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0]["content"].as_str().unwrap().contains("databases replication"));
    assert_eq!(env["metadata"]["total_chunks"], 1);
}

#[tokio::test]
async fn unknown_item_id_is_a_structured_error() {
    let server = server_with_memory().await;
    let raw = server
        .memorabilia_read_item(Parameters(ReadItemRequest {
            item_id: "p_not_a_real_item".into(),
            offset: None,
            limit: None,
        }))
        .await;
    let env: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(env["status"], "error");
    assert_eq!(env["error_code"], "MEMORABILIA_ITEM_NOT_FOUND");
    assert!(env["hint"].as_str().unwrap().contains("memorabilia_search"));
}

#[tokio::test]
async fn empty_query_is_a_soft_success() {
    let server = server_with_memory().await;
    let raw = server
        .memorabilia_search(Parameters(SearchRequest {
            query: "   ".into(),
            offset: None,
            limit: None,
        }))
        .await;
    let env: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(env["status"], "success");
    assert!(env["data"].as_array().unwrap().is_empty());
    assert!(env["message"].as_str().unwrap().contains("empty"));
}
