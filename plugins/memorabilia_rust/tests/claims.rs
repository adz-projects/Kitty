//! Phase 4.2-4.4: claims carry attribution and stance, paraphrases merge,
//! and claims are disputed or superseded one at a time.

#![allow(clippy::field_reassign_with_default)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use memorabilia::config::Config;
use memorabilia::embed::HashEmbedder;
use memorabilia::engine::Engine;
use memorabilia::learn::IngestInput;
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::text::{normalize_text, sha256_hex};
use memorabilia::traits::{Embedder, StructuredChat};

const T0: &str = "2026-08-01T10:00:00Z";
const SPACE: &str = "test:semantic";

/// The lexical hash embedder under a semantic-looking tag: claim
/// consolidation deliberately skips the hash space, so tests that exercise
/// it need a tag that is not `__lexical_hash__`.
struct TaggedHash(HashEmbedder);

#[async_trait]
impl Embedder for TaggedHash {
    async fn embed(&self, text: &str) -> Result<(Vec<f32>, String), String> {
        let (v, _) = self.0.embed(text).await?;
        Ok((v, SPACE.to_string()))
    }
    fn model_tag(&self) -> &str {
        SPACE
    }
}

/// Answers extraction by the chunk's source, and the paraphrase check by
/// confirming every pair.
struct Scripted {
    by_source: HashMap<String, Value>,
}

#[async_trait]
impl StructuredChat for Scripted {
    async fn structured_chat(&self, messages: Vec<Value>, _schema: &Value) -> Result<Value, String> {
        let user = messages
            .iter()
            .rev()
            .find_map(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        if user.contains("NEW: ") {
            let n = user.matches("NEW: ").count();
            let same: Vec<Value> = (0..n).map(|i| json!({"pair": i, "same": true})).collect();
            return Ok(json!({ "same": same }));
        }
        let source = user
            .trim_start_matches("CHUNK (source_entity: ")
            .split(')')
            .next()
            .unwrap_or_default();
        self.by_source.get(source).cloned().ok_or_else(|| format!("no script for {source}"))
    }
}

fn node(claim: &str) -> String {
    format!("p_{}", &sha256_hex(&normalize_text(claim))[..24])
}

fn claim(text: &str) -> Value {
    json!({"claim": text, "importance": "medium", "urgency": "low", "decay_class": "transient",
           "urgency_expires_at": "", "attribution": "", "stance": "states"})
}

fn doc(content: &str, source: &str) -> IngestInput {
    IngestInput {
        content: content.into(),
        source_type: "Scraped".into(),
        source_name: source.into(),
        source_entity: source.into(),
        captured_at: T0.into(),
        intent: None,
    }
}

async fn engine(cfg: Config, script: Vec<(&str, Value)>) -> Engine {
    let mut cfg = cfg;
    cfg.embedding_model = SPACE.into();
    cfg.conflict_probe_similarity_min = -1.0; // every other chunk is a probe neighbor
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(cfg.embedding_dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    let chat = Arc::new(Scripted {
        by_source: script.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    });
    let embedder = Arc::new(TaggedHash(HashEmbedder::new(cfg.embedding_dim)));
    Engine::new(cfg, db, chat, embedder, vectors)
}

async fn drain(e: &Engine) {
    for _ in 0..10 {
        if e.drain_extraction(T0).await.attempted == 0 {
            break;
        }
    }
}

#[tokio::test]
async fn a_superseded_claim_leaves_recall_but_stays_readable() {
    let old = "The Hartfield Bridge is closed to traffic for structural repairs";
    let new = "The Hartfield Bridge reopened to traffic in March 2026";
    // With two chunks, mean-centring makes them mirror images, so the
    // relevance floor would cut one of them whatever the query; supersession,
    // not the floor, is under test here.
    let mut cfg = Config::default();
    cfg.recall_min_cosine = -1.0;
    cfg.recall_gap_min = 9.9;
    let e = engine(cfg, vec![
        ("citynews-2025", json!({"propositions": [claim(old)], "relations": []})),
        ("citynews-2026", json!({"propositions": [claim(new)], "relations": [
            {"my_claim": new, "other_claim_id": node(old), "kind": "supersedes", "strength": 0.9, "reason": "reopened"}
        ]})),
    ])
    .await;
    e.ingest(&doc("The Hartfield Bridge is closed to all traffic for structural repairs this winter.", "citynews-2025")).await;
    drain(&e).await;
    e.ingest(&doc("The Hartfield Bridge reopened to traffic in March 2026 after its repairs.", "citynews-2026")).await;
    drain(&e).await;

    let block = e.recall("Is the Hartfield Bridge open to traffic?").await.unwrap();
    assert!(block.contains("reopened to traffic"), "{block}");
    assert!(!block.contains("closed to traffic"), "the superseded claim must not be recalled: {block}");
    let view = e.render_item(&node(old), 0, 10).await.expect("still readable");
    assert!(view.claim.contains("superseded") && view.claim.contains("reopened"), "{}", view.claim);
}

#[tokio::test]
async fn a_dispute_shows_both_sides_and_feeds_the_counterpoint() {
    let pro = "AI tutors improve learning outcomes for students";
    let con = "AI tutors that hand out answers reduce what students learn";
    let mut argued = claim(pro);
    argued["attribution"] = json!("EdTech Weekly");
    argued["stance"] = json!("argues");
    let e = engine(Config::default(), vec![
        ("edtech", json!({"propositions": [argued], "relations": []})),
        ("study", json!({"propositions": [claim(con)], "relations": [
            {"my_claim": con, "other_claim_id": node(pro), "kind": "disputes", "strength": 0.8, "reason": "opposite findings"}
        ]})),
    ])
    .await;
    e.ingest(&doc("EdTech Weekly argues that AI tutors improve learning outcomes for students everywhere.", "edtech")).await;
    drain(&e).await;
    e.ingest(&doc("A study found that AI tutors that hand out answers reduce what students learn.", "study")).await;
    drain(&e).await;

    let block = e.recall("Do AI tutors improve learning outcomes for students?").await.unwrap();
    assert!(block.contains("EdTech Weekly argues:"), "an argument is attributed, not stated as fact: {block}");
    assert!(block.contains("(disputed by ["), "both sides are shown: {block}");
    let counter = e.counterpoint("AI tutors improve learning outcomes for students").await.expect("a counter-view");
    assert!(counter.claim.contains("reduce what students learn"), "{counter:?}");
    assert!(!counter.citation.is_empty());
}

#[tokio::test]
async fn a_confirmed_paraphrase_supports_the_existing_claim() {
    let a = "Solar panels lose about half a percent of efficiency each year";
    let b = "Solar panels lose roughly half a percent of their efficiency every year";
    let mut cfg = Config::default();
    cfg.claim_merge_cosine = 0.3; // lexical vectors: make the paraphrase a candidate
    let e = engine(cfg, vec![
        ("energy-a", json!({"propositions": [claim(a)], "relations": []})),
        ("energy-b", json!({"propositions": [claim(b)], "relations": []})),
    ])
    .await;
    e.ingest(&doc("Solar panels lose about half a percent of efficiency each year of operation.", "energy-a")).await;
    drain(&e).await;
    let single = e.db.get_proposition(&node(a)).await.unwrap().unwrap().confidence;
    e.ingest(&doc("Solar panels lose roughly half a percent of their efficiency every year, studies say.", "energy-b")).await;
    drain(&e).await;

    assert!(e.db.get_proposition(&node(b)).await.unwrap().is_none(), "the paraphrase did not become its own claim");
    let merged = e.db.get_proposition(&node(a)).await.unwrap().unwrap();
    assert_eq!(e.db.list_active_supporting_chunks(&node(a)).await.unwrap().len(), 2);
    assert!(merged.confidence > single, "two sources corroborate: {} > {single}", merged.confidence);
}
