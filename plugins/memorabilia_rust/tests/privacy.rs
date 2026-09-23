//! Privacy layer (plan §15 Phase 3, §12).
//!
//! Build-failing guards pinned here: the Stage 0 PII gate rejects before any
//! write and audits the category only (never the value); the `private`
//! forget cascade removes chunk/vector/links/edges in one transaction and
//! tombstones the text so it is never relearned; short generic phrases are
//! deleted without poisoning universal patterns; `wrong` suppresses
//! permanently + tombstones; `outdated` suppresses for
//! `outdated_suppression_days` with no tombstone.

use std::sync::Arc;

use memorabilia::config::Config;
use memorabilia::embed::HashEmbedder;
use memorabilia::engine::Engine;
use memorabilia::learn::IngestInput;
use memorabilia::privacy::{
    ForgetReason, MockPiiClassifier, PiiCategory, scan_pii, should_tombstone,
};
use memorabilia::store::propositions::Proposition;
use memorabilia::store::tombstones::Suppression;
use memorabilia::store::vectors::SqliteVectorIndex;
use memorabilia::store::Db;
use memorabilia::traits::MockChat;
use sqlx::SqlitePool;

const T0: &str = "2026-08-01T10:00:00Z";

async fn test_engine(cfg: Config) -> Engine {
    let dim = cfg.embedding_dim;
    let db = Db::open_in_memory().await.unwrap();
    db.init_vectors(dim).await.unwrap();
    let vectors = Arc::new(SqliteVectorIndex::new(db.pool().clone()));
    Engine::new(
        cfg,
        db,
        Arc::new(MockChat {
            response: serde_json::json!({}),
        }),
        Arc::new(HashEmbedder::new(dim)),
        vectors,
    )
}

fn note(content: &str) -> IngestInput {
    IngestInput {
        content: content.into(),
        source_type: "UserNote".into(),
        source_name: "notes".into(),
        source_entity: "user".into(),
        captured_at: T0.into(),
        intent: None,
    }
}

async fn count_of(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// ---------------------------------------------------------------------------
// Deterministic detectors (pure)
// ---------------------------------------------------------------------------

#[test]
fn detectors_flag_each_pii_category() {
    let has = |cats: Vec<PiiCategory>, c: PiiCategory| {
        assert!(cats.contains(&c), "expected {c:?} in {cats:?}");
    };
    let email = scan_pii("email jane.doe@example.com today");
    has(email.clone(), PiiCategory::Email);
    let ssn = scan_pii("ssn is 123-45-6789 on file");
    has(ssn.clone(), PiiCategory::Ssn);
    let card = scan_pii("card 4111 1111 1111 1111 stored");
    has(card.clone(), PiiCategory::Card);
    let key = scan_pii("token sk-abcdefghijklmnopqrstuvwxyz123456 end");
    has(key.clone(), PiiCategory::ApiKey);
    let phone = scan_pii("call 415-555-0132 now");
    has(phone.clone(), PiiCategory::Phone);
    let addr = scan_pii("lived at 742 evergreen street back in the day");
    has(addr.clone(), PiiCategory::Address);
    // exact single-category payloads stay single-category
    assert_eq!(email, vec![PiiCategory::Email]);
    assert_eq!(ssn, vec![PiiCategory::Ssn]);
    assert_eq!(card, vec![PiiCategory::Card]);
    assert_eq!(key, vec![PiiCategory::ApiKey]);
    assert_eq!(phone, vec![PiiCategory::Phone]);
    assert_eq!(addr, vec![PiiCategory::Address]);
}

#[test]
fn combined_payload_reports_every_category_in_scan_order() {
    // samples are separated by strong punctuation: space/dash are *internal*
    // separators for the card/phone runs, so adjacent digit samples would
    // otherwise fuse into one run
    let cats = scan_pii(
        "email j@e.co; ssn 123-45-6789. card 4111 1111 1111 1111; \
         key sk-abcdefghijklmnopqrstuvwxyz123456; phone 415-555-0132; \
         address 742 evergreen street",
    );
    assert_eq!(
        cats,
        vec![
            PiiCategory::Email,
            PiiCategory::Ssn,
            PiiCategory::Card,
            PiiCategory::ApiKey,
            PiiCategory::Phone,
            PiiCategory::Address,
        ]
    );
}

#[test]
fn detectors_miss_timestamps_versions_and_prose() {
    // 10/11-digit grouped timestamps and semver must not read as phone/card;
    // the address scan must not fire on them either.
    assert_eq!(
        scan_pii("released 2026-08-20 10:00:00 in version 1.2.3"),
        Vec::<PiiCategory>::new()
    );
    assert_eq!(scan_pii("the bridge over the river was rebuilt"), Vec::new());
    // SSN shape is exact: 4-2-3 and 3-3-3 arrangements are not SSNs. (The
    // 3-3-4 arrangement `123-456-7890` is deliberately NOT pinned empty: it
    // is also a legal NANP phone shape, and the gate errs conservative.)
    assert_eq!(scan_pii("id 1234-56-7890 ok"), Vec::new());
    assert_eq!(scan_pii("id 123-456-789 ok"), Vec::new());
    // Luhn must hold: this 16-digit run fails the check.
    assert_eq!(scan_pii("card 1234567890123456 stored"), Vec::new());
    // Bare high-entropy key runs need mixed case + digits (pure hex is not one).
    assert_eq!(
        scan_pii("digest 6b86b273ff34fce19d6b804eff5a3f5b4fa877186ba1e37c2883daf47ed4cb59 checked"),
        Vec::new()
    );
}

#[test]
fn should_tombstone_boundaries() {
    let cfg = Config::default(); // tombstone_min_chars = 20, digit run 8
    // length boundary: 19 fails, 20 clears
    assert!(!should_tombstone("abcdefghijklmnopqrs", &cfg));
    assert!(should_tombstone("abcdefghijklmnopqrst", &cfg));
    // short text with an 8-digit run is high-entropy; 7 digits is not
    assert!(should_tombstone("abc 12345678 xyz", &cfg));
    assert!(!should_tombstone("abc 1234567 xyz", &cfg));
    // any PII pattern short-circuits the length floor
    assert!(should_tombstone("wipe a@b.co", &cfg));
}

// ---------------------------------------------------------------------------
// Stage 0 gate (via the engine)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pii_rejection_writes_nothing_and_audits_category_only() {
    let engine = test_engine(Config::default()).await;
    let out = engine.ingest(&note("contact jane.doe@example.com for the account details please")).await;

    assert_eq!(
        out,
        memorabilia::learn::IngestOutcome {
            rejected_pii: vec!["email".into()],
            ..Default::default()
        }
    );

    // nothing reached disk: no chunks, no document row
    assert_eq!(count_of(engine.db.pool(), "SELECT count(*) FROM chunks").await, 0);
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM documents").await,
        0
    );

    // the audit row carries the category only — never the value
    let rows = engine.db.list_audit_by_event("rejected:pii").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].category.as_deref(), Some("email"));
    let blob = format!(
        "{}|{}|{}",
        rows[0].event,
        rows[0].category.clone().unwrap_or_default(),
        rows[0].detail.clone().unwrap_or_default()
    );
    assert!(!blob.contains("jane.doe@example.com"));
}

#[tokio::test]
async fn pii_classifier_adds_category_and_soft_fails_on_error() {
    // a clean classifier adds its category on otherwise-clean text
    let engine = test_engine(Config::default())
        .await
        .with_pii_classifier(Arc::new(MockPiiClassifier {
        category: Some("name".into()),
        error: None,
    }));
    let out = engine
        .ingest(&note("just a person named bob shared this with me"))
        .await;
    assert_eq!(out.rejected_pii, vec!["name".to_string()]);
    let rows = engine.db.list_audit_by_event("rejected:pii").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].category.as_deref(), Some("name"));

    // a broken classifier never overrides the deterministic result
    let engine = test_engine(Config::default())
        .await
        .with_pii_classifier(Arc::new(MockPiiClassifier {
        category: None,
        error: Some("classifier down".into()),
    }));
    let out = engine
        .ingest(&note("contact jane.doe@example.com for the account"))
        .await;
    assert_eq!(out.rejected_pii, vec!["email".to_string()]);
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM chunks").await,
        0
    );
}

// ---------------------------------------------------------------------------
// Forget ladder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn private_forget_cascades_and_tombstones_never_relearn() {
    let engine = test_engine(Config::default()).await;
    let content = "build 20260816 was cut off after the deploy";
    assert_eq!(engine.ingest(&note(content)).await.chunks_written, 1);
    engine
        .ingest(&note("unrelated second note about the harbor wall"))
        .await;

    // wire up one proposition the victim chunk supports (importance high →
    // orphan rule archives it) plus a DISPUTED edge to the other chunk, so
    // the cascade has to drop the edge row by FK
    let active = engine.db.list_chunks_by_status("active").await.unwrap();
    let chunk = active.iter().find(|c| c.content == content).cloned().unwrap();
    let other = active.iter().find(|c| c.content != content).cloned().unwrap();
    let (a, b) = if chunk.chunk_id < other.chunk_id {
        (chunk.chunk_id.clone(), other.chunk_id.clone())
    } else {
        (other.chunk_id.clone(), chunk.chunk_id.clone())
    };
    let node = "n_1";
    engine
        .db
        .insert_proposition(&Proposition {
            node_id: node.into(),
            claim: "the build pipeline cut off after deploys".into(),
            confidence: 0.5,
            is_disputed: true,
            status: "active".into(),
            importance: "high".into(),
            urgency: "medium".into(),
            urgency_expires_at: None,
            last_assessed_at: None,
            archived_at: None,
            created_at: T0.into(),
        })
        .await
        .unwrap();
    engine.db.add_support_link(&chunk.chunk_id, node, T0).await.unwrap();
    engine
        .db
        .insert_disputed_edge(&memorabilia::store::edges::DisputedEdge {
            edge_id: "d_1".into(),
            chunk_a: a,
            chunk_b: b,
            strength: 0.9,
            opened_at: T0.into(),
            closed_at: None,
            reason: "test dispute".into(),
        })
        .await
        .unwrap();
    let (ch_hash, doc_hash) = (chunk.content_hash.clone(), chunk.document_hash.clone());

    let n = engine.forget(content, ForgetReason::Private).await;
    assert_eq!(n, 1);

    // the cascade removed everything the victim chunk owned; the unrelated
    // chunk survives untouched
    assert_eq!(count_of(engine.db.pool(), "SELECT count(*) FROM chunks").await, 1);
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM chunks_vec").await,
        1
    );
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM chunk_propositions").await,
        0
    );
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM disputed_edges").await,
        0
    );
    // the orphaned high-importance proposition is archived, not deleted
    let p = engine.db.get_proposition(node).await.unwrap().expect("proposition row");
    assert_eq!(p.status, "UNSUPPORTED_ARCHIVE");
    assert!(p.archived_at.is_some());

    // permanent content + document tombstones: the text is never relearned
    assert!(engine.db.has_tombstone(&ch_hash).await.unwrap());
    assert!(engine.db.has_tombstone(&doc_hash).await.unwrap());

    // audit: reason + structural count only
    let rows = engine.db.list_audit_by_event("deleted:private").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].detail.as_deref(), Some("chunks=1"));
    assert!(!rows[0].detail.as_deref().unwrap().contains(content));

    // re-ingesting the identical payload aborts at the document tombstone
    let out = engine.ingest(&note(content)).await;
    assert!(out.aborted_document_seen);
    assert_eq!(out.chunks_written, 0);

    // a different payload whose normalized chunk text matches the forgotten
    // text is caught by the content tombstone
    let out = engine.ingest(&note(&format!("{content}."))).await;
    assert_eq!(out.chunks_written, 0);
    assert_eq!(out.chunks_skipped, 1);
    assert_eq!(count_of(engine.db.pool(), "SELECT count(*) FROM chunks").await, 1);
}

#[tokio::test]
async fn short_generic_phrase_deletes_without_tombstone() {
    let engine = test_engine(Config::default()).await;
    let content = "call me tomorrow"; // 16 chars < tombstone_min_chars
    assert_eq!(engine.ingest(&note(content)).await.chunks_written, 1);

    assert_eq!(engine.forget(content, ForgetReason::Private).await, 1);
    assert_eq!(count_of(engine.db.pool(), "SELECT count(*) FROM chunks").await, 0);
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM tombstones").await,
        0
    );

    // without a tombstone the (rephrased) text may legally come back
    let out = engine.ingest(&note("call me tomorrow.")).await;
    assert_eq!(out.chunks_written, 1);
}

#[tokio::test]
async fn wrong_forget_is_permanent_suppression_with_tombstone() {
    let engine = test_engine(Config::default()).await;
    let content = "the bridge closed every friday for maintenance";
    assert_eq!(engine.ingest(&note(content)).await.chunks_written, 1);
    let chunk = engine.db.list_chunks_by_status("active").await.unwrap().pop().unwrap();

    assert_eq!(engine.forget(content, ForgetReason::Wrong).await, 1);

    // rows remain (evidence of correction) — the chunk is only suppressed
    assert!(engine.db.get_chunk(&chunk.chunk_id).await.unwrap().is_some());
    let s: Option<Suppression> =
        sqlx::query_as("SELECT * FROM suppressions WHERE chunk_id = ?")
            .bind(&chunk.chunk_id)
            .fetch_optional(engine.db.pool())
            .await
            .unwrap();
    let s = s.expect("suppression row");
    assert_eq!(s.reason, "wrong");
    assert!(s.permanent);
    assert_eq!(s.expires_at, None);

    // suppressed forever, and tombstoned like `private`
    let suppressed = engine
        .db
        .list_suppressed_chunk_ids("2999-12-31T23:59:59Z")
        .await
        .unwrap();
    assert!(suppressed.contains(&chunk.chunk_id));
    assert!(engine.db.has_tombstone(&chunk.content_hash).await.unwrap());
    assert!(engine.db.has_tombstone(&chunk.document_hash).await.unwrap());

    // wrong text must never resurface either
    let out = engine.ingest(&note(content)).await;
    assert!(out.aborted_document_seen);
}

#[tokio::test]
async fn outdated_forget_is_time_bounded_suppression_without_tombstone() {
    let mut cfg = Config::default();
    cfg.outdated_suppression_days = 5;
    let engine = test_engine(cfg).await;
    let content = "the quarterly report lands on the fifteenth";
    assert_eq!(engine.ingest(&note(content)).await.chunks_written, 1);
    let chunk = engine.db.list_chunks_by_status("active").await.unwrap().pop().unwrap();

    assert_eq!(engine.forget(content, ForgetReason::Outdated).await, 1);

    let s: Suppression =
        sqlx::query_as("SELECT * FROM suppressions WHERE chunk_id = ?")
            .bind(&chunk.chunk_id)
            .fetch_one(engine.db.pool())
            .await
            .unwrap();
    assert_eq!(s.reason, "outdated");
    assert!(!s.permanent);
    // NaiveDateTime: the stored format is a UTC wall-clock string; the
    // trailing literal Z must not be asked to carry an offset.
    let fmt = "%Y-%m-%dT%H:%M:%SZ";
    let created = chrono::NaiveDateTime::parse_from_str(&s.created_at, fmt).unwrap();
    let expires =
        chrono::NaiveDateTime::parse_from_str(&s.expires_at.clone().unwrap(), fmt).unwrap();
    assert_eq!(expires - created, chrono::Duration::days(5));

    // suppressed until the expiry, then legally re-learnable
    let at = |d: chrono::NaiveDateTime| d.format(fmt).to_string();
    let still = engine
        .db
        .list_suppressed_chunk_ids(&at(created + chrono::Duration::days(4)))
        .await
        .unwrap();
    assert!(still.contains(&chunk.chunk_id));
    let gone = engine
        .db
        .list_suppressed_chunk_ids(
            &at(created + chrono::Duration::days(5) + chrono::Duration::seconds(2)),
        )
        .await
        .unwrap();
    assert!(!gone.contains(&chunk.chunk_id));

    // no tombstone: the fact may come back as current
    assert!(!engine.db.has_tombstone(&chunk.content_hash).await.unwrap());
    assert!(!engine.db.has_tombstone(&chunk.document_hash).await.unwrap());
}

#[tokio::test]
async fn forget_threshold_boundary_miss_then_total_match() {
    // default threshold (0.8): an unrelated phrase matches nothing
    let engine = test_engine(Config::default()).await;
    engine
        .ingest(&note("zebrafinch tank lights were replaced on tuesday morning"))
        .await;
    engine
        .ingest(&note("the pasta recipe calls for three cups of flour total"))
        .await;
    assert_eq!(
        engine
            .forget("completely unrelated query about the weather", ForgetReason::Private)
            .await,
        0
    );
    let rows = engine.db.list_audit_by_event("deleted:private").await.unwrap();
    assert_eq!(
        rows[0].detail.as_deref(),
        Some("unresolved: no matching active chunk")
    );
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM chunks").await,
        2
    );

    // injected threshold -1.0: every active chunk clears the (impossible) floor
    let mut cfg = Config::default();
    cfg.forget_match_threshold = -1.0;
    let engine = test_engine(cfg).await;
    engine
        .ingest(&note("zebrafinch tank lights were replaced on tuesday morning"))
        .await;
    engine
        .ingest(&note("the pasta recipe calls for three cups of flour total"))
        .await;
    assert_eq!(
        engine
            .forget("completely unrelated query about the weather", ForgetReason::Private)
            .await,
        2
    );
    assert_eq!(
        count_of(engine.db.pool(), "SELECT count(*) FROM chunks").await,
        0
    );
}

#[tokio::test]
async fn forget_empty_phrase_is_a_noop_audit() {
    let engine = test_engine(Config::default()).await;
    assert_eq!(
        engine.forget("   ", ForgetReason::Private).await,
        0
    );
    let rows = engine.db.list_audit_by_event("deleted:private").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].detail.as_deref(),
        Some("unresolved: no matching active chunk")
    );
}

// ---------------------------------------------------------------------------
// Legacy dialogue purge (0.11.4)
// ---------------------------------------------------------------------------

fn sourced(content: &str, source_type: &str, source: &str) -> IngestInput {
    IngestInput {
        content: content.into(),
        source_type: source_type.into(),
        source_name: source.into(),
        source_entity: source.into(),
        captured_at: T0.into(),
        intent: None,
    }
}

fn claim(node: &str, text: &str) -> Proposition {
    Proposition {
        node_id: node.into(),
        claim: text.into(),
        confidence: 0.35,
        is_disputed: false,
        status: "active".into(),
        // `high` would only be *archived* by forget's orphan rule; the purge
        // must delete it outright — dialogue claims were never facts.
        importance: "high".into(),
        urgency: "unknown".into(),
        urgency_expires_at: None,
        last_assessed_at: None,
        archived_at: None,
        created_at: T0.into(),
    }
}

/// Regression: evidence harvested from chat dialogue by the retired
/// message-pair harvest (e.g. the assistant listing its own tools) turned into
/// "facts" like "The system has functions for …". The one-time purge removes
/// that evidence and its claims, leaves real documents alone, and runs once.
#[tokio::test]
async fn legacy_conversation_purge_removes_dialogue_and_keeps_documents() {
    let engine = test_engine(Config::default()).await;
    let dialogue = "assistant: here is a complete list of my tools and what each one does";
    let page = "The harbor wall was rebuilt in 1987 after the storm surge damaged it";
    assert_eq!(
        engine
            .ingest(&sourced(dialogue, "Conversation", "session:s1"))
            .await
            .chunks_written,
        1
    );
    assert_eq!(
        engine
            .ingest(&sourced(page, "Scraped", "example.org"))
            .await
            .chunks_written,
        1
    );
    let active = engine.db.list_chunks_by_status("active").await.unwrap();
    let conv = active.iter().find(|c| c.content == dialogue).unwrap().chunk_id.clone();
    let doc = active.iter().find(|c| c.content == page).unwrap().chunk_id.clone();
    for (node, text, chunk) in [
        ("n_conv", "The system has functions for listing its tools", &conv),
        ("n_doc", "The harbor wall was rebuilt in 1987", &doc),
    ] {
        engine.db.insert_proposition(&claim(node, text)).await.unwrap();
        engine.db.add_support_link(chunk, node, T0).await.unwrap();
    }

    engine.purge_legacy_conversation_documents().await;

    let pool = engine.db.pool();
    assert_eq!(
        count_of(pool, "SELECT COUNT(*) FROM documents WHERE source_type = 'Conversation'").await,
        0
    );
    assert_eq!(
        count_of(pool, "SELECT COUNT(*) FROM documents WHERE source_type = 'Scraped'").await,
        1
    );
    assert!(engine.db.get_proposition("n_conv").await.unwrap().is_none());
    assert!(engine.db.get_proposition("n_doc").await.unwrap().is_some());
    let vec_rows = |id: &str| format!("SELECT COUNT(*) FROM chunks_vec WHERE chunk_id = '{id}'");
    assert_eq!(count_of(pool, &vec_rows(&conv)).await, 0, "dialogue vector removed");
    assert_eq!(count_of(pool, &vec_rows(&doc)).await, 1, "document vector kept");
    let audited = "SELECT COUNT(*) FROM audit_log WHERE event = 'purged:legacy_conversation'";
    assert_eq!(count_of(pool, audited).await, 1);

    // Runs once: a second call is a no-op (flag set, no second audit row).
    engine.purge_legacy_conversation_documents().await;
    assert_eq!(count_of(pool, audited).await, 1);
}
