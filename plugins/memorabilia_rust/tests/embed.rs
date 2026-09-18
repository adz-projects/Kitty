//! Phase 1 embedding layer (plan §15 Phase 1, §3.3): determinism, unit
//! normalization, timeout → hash fallback, LRU cache space tagging,
//! circuit-breaker probing, `reembed_stale` candidate selection, and the
//! cross-space regression guard. No test requires the network: the
//! "Ollama" endpoint is a raw-TCP responder bound to 127.0.0.1:0 inside
//! this test process.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use memorabilia::config::{Config, DEFAULT_EMBEDDING_MODEL, HASH_EMBED_MODEL};
use memorabilia::embed::{hash_embed, project, HashEmbedder, OllamaEmbedder};
use memorabilia::traits::{Embedder, MemoryVectorIndex, VectorIndex};

// ---------------------------------------------------------------------------
// In-process mock Ollama.
// ---------------------------------------------------------------------------

/// One scripted HTTP response: status line, JSON body, and an optional delay
/// before the response is written (to outlast `embedding_timeout_ms`).
struct Response {
    status: u16,
    body: Vec<u8>,
    delay_ms: u64,
}

type Handler = dyn Fn(&[u8]) -> Response + Send + Sync;

struct MockOllama {
    url: String,
    hits: Arc<AtomicUsize>,
    _task: tokio::task::JoinHandle<()>,
}

impl MockOllama {
    async fn start(handler: Box<Handler>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let handler: Arc<Handler> = handler.into();
        let hits_task = hits.clone();
        let task = tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let handler = handler.clone();
                let hits = hits_task.clone();
                tokio::spawn(async move {
                    let _ = handle_one(sock, &handler, &hits).await;
                });
            }
        });
        Self {
            url: format!("http://{addr}"),
            hits,
            _task: task,
        }
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn handle_one(
    mut sock: tokio::net::TcpStream,
    handler: &Arc<Handler>,
    hits: &Arc<AtomicUsize>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read until the end of the headers, then the Content-Length body bytes.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        match sock.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos;
                }
            }
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let content_length = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let body_end = header_end + 4 + content_length;
    while buf.len() < body_end {
        match sock.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    hits.fetch_add(1, Ordering::SeqCst);
    let body_start = header_end + 4;
    let body = &buf[body_start..buf.len().min(body_end)];
    let resp = handler(body);
    if resp.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(resp.delay_ms)).await;
    }
    let reason = if resp.status == 200 { "OK" } else { "Internal Server Error" };
    let mut out = format!("HTTP/1.1 {} {}\r\n", resp.status, reason).into_bytes();
    out.extend_from_slice(b"Content-Type: application/json\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(&resp.body);
    let _ = sock.write_all(&out).await;
    let _ = sock.shutdown().await;
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn cfg_for(url: &str) -> Config {
    let mut c = Config::default();
    c.embedding_provider_url = url.to_string();
    c
}

// ---------------------------------------------------------------------------
// Hash embedder (deterministic fallback space).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_text_identical_vector_and_hash_tag() {
    let e = HashEmbedder::new(Config::default().embedding_dim);
    let (a, tag_a) = e.embed("the ssl certificate expires 2026-09-01").await.unwrap();
    let (b, tag_b) = e.embed("the ssl certificate expires 2026-09-01").await.unwrap();
    assert_eq!(a, b);
    assert_eq!(tag_a, HASH_EMBED_MODEL);
    assert_eq!(tag_b, HASH_EMBED_MODEL);
}

#[tokio::test]
async fn hash_vector_is_dim_correct_and_unit_norm() {
    let c = Config::default();
    let e = HashEmbedder::new(c.embedding_dim);
    let (v, _) = e.embed("deployment window is friday 10pm").await.unwrap();
    assert_eq!(v.len(), c.embedding_dim);
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((n - 1.0).abs() < 1e-4);
}

#[tokio::test]
async fn empty_text_is_the_zero_vector_tagged_hash() {
    let e = HashEmbedder::new(16);
    let (v, tag) = e.embed("   ").await.unwrap();
    assert!(v.iter().all(|x| *x == 0.0));
    assert_eq!(tag, HASH_EMBED_MODEL);
}

// ---------------------------------------------------------------------------
// Ollama provider: timeout → fallback, never a hard failure.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unreachable_endpoint_falls_back_to_hash_space_without_error() {
    // A port nothing listens on: connect fails fast, well inside the
    // budget. No error may escape (soft-fail), and the vector must be the
    // deterministic fallback for the exact text.
    let mut c = cfg_for("http://127.0.0.1:1");
    c.embedding_timeout_ms = 80;
    let p = OllamaEmbedder::new(&c);
    let (v, tag) = p.embed("quarterly revenue exceeded 4 million").await.unwrap();
    assert_eq!(tag, HASH_EMBED_MODEL);
    assert_eq!(v, hash_embed("quarterly revenue exceeded 4 million", c.embedding_dim));
    assert!(p.probe_semantic().await == false);
}

#[tokio::test]
async fn server_slower_than_the_budget_falls_back() {
    // Accepts then stalls far past embedding_timeout_ms.
    let server = MockOllama::start(Box::new(|_body| Response {
        status: 200,
        body: br#"{"embedding":[0.1,0.2,0.3]}"#.to_vec(),
        delay_ms: 400,
    }))
    .await;
    let mut c = cfg_for(&server.url);
    c.embedding_timeout_ms = 80;
    let p = OllamaEmbedder::new(&c);
    let (v, tag) = p.embed("backup job finished without errors").await.unwrap();
    assert_eq!(tag, HASH_EMBED_MODEL);
    assert_eq!(v, hash_embed("backup job finished without errors", c.embedding_dim));
}

// ---------------------------------------------------------------------------
// Ollama provider: success path, projection, caching, space tags.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_success_is_tagged_semantic_and_projected() {
    // A 9-dim native width forces the wrap-add projection to dim 8.
    let raw9: Vec<f32> = (1..=9).map(|i| (i as f32) / 9.0).collect();
    let raw_json = serde_json::json!({ "embedding": raw9 });
    let server = MockOllama::start(Box::new(move |_body| Response {
        status: 200,
        body: serde_json::to_vec(&raw_json).unwrap(),
        delay_ms: 0,
    }))
    .await;
    let mut c = cfg_for(&server.url);
    c.embedding_dim = 8;
    let p = OllamaEmbedder::new(&c);
    let (v, tag) = p.embed("the load balancer drops idle connections after 60s").await.unwrap();
    assert_eq!(tag, c.embedding_model);
    assert_eq!(v, project(&raw9, 8));
    assert!(p.probe_semantic().await);
}

#[tokio::test]
async fn cache_hit_serves_the_vector_and_its_actual_space() {
    let body = serde_json::json!({ "embedding": [0.3, -0.4, 0.5] });
    let server = MockOllama::start(Box::new(move |_body| Response {
        status: 200,
        body: serde_json::to_vec(&body).unwrap(),
        delay_ms: 0,
    }))
    .await;
    let c = cfg_for(&server.url);
    let p = OllamaEmbedder::new(&c);
    let (v1, tag1) = p.embed("rotate the api keys quarterly").await.unwrap();
    assert_eq!(tag1, c.embedding_model);
    let (v2, tag2) = p.embed("rotate the api keys quarterly").await.unwrap();
    assert_eq!(v1, v2);
    assert_eq!(tag2, c.embedding_model);
    // The second call must not have hit the endpoint.
    assert_eq!(server.hit_count(), 1);
}

// ---------------------------------------------------------------------------
// Circuit breaker: fast fallback while down, re-probe after the interval.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn breaker_suppresses_retries_within_the_probe_interval() {
    // Always "up" HTTP-wise but never returns a usable `embedding` field.
    let server = MockOllama::start(Box::new(|_body| Response {
        status: 200,
        body: b"{}".to_vec(),
        delay_ms: 0,
    }))
    .await;
    let mut c = cfg_for(&server.url);
    c.embedding_probe_interval_s = 60; // default; must not fire during the test
    let p = OllamaEmbedder::new(&c);
    let (v1, tag1) = p.embed("first text about the database migration").await.unwrap();
    assert_eq!(tag1, HASH_EMBED_MODEL);
    assert_eq!(v1, hash_embed("first text about the database migration", c.embedding_dim));
    // A second cache-miss text must fast-fall back WITHOUT another request.
    let (_, tag2) = p.embed("second text about the database migration").await.unwrap();
    assert_eq!(tag2, HASH_EMBED_MODEL);
    assert_eq!(server.hit_count(), 1, "breaker must suppress the retry");
    // A third text, still inside the probe interval.
    let (_, tag3) = p.embed("third text about the database migration").await.unwrap();
    assert_eq!(tag3, HASH_EMBED_MODEL);
    assert_eq!(server.hit_count(), 1);
}

#[tokio::test]
async fn breaker_reprobes_after_the_configured_interval() {
    let server = MockOllama::start(Box::new(|_body| Response {
        status: 200,
        body: b"{}".to_vec(),
        delay_ms: 0,
    }))
    .await;
    let mut c = cfg_for(&server.url);
    c.embedding_probe_interval_s = 1;
    let p = OllamaEmbedder::new(&c);
    let _ = p.embed("alpha row of the ledger").await.unwrap();
    assert_eq!(server.hit_count(), 1);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let _ = p.embed("beta row of the ledger").await.unwrap();
    assert_eq!(server.hit_count(), 2, "re-probe after the interval");
}

// ---------------------------------------------------------------------------
// reembed_stale: probe → fresh path bypasses the poisoned cache entry.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fresh_bypasses_stale_hash_vector_and_recovers_cache() {
    // First request has no usable `embedding` field; every request after
    // that succeeds. Stateful, like an outage that recovers.
    let flips = Arc::new(AtomicUsize::new(0));
    let server = MockOllama::start(Box::new({
        let flips = flips.clone();
        move |_body| {
            if flips.fetch_add(1, Ordering::SeqCst) == 0 {
                Response {
                    status: 200,
                    body: b"{}".to_vec(),
                    delay_ms: 0,
                }
            } else {
                Response {
                    status: 200,
                    body: serde_json::to_vec(&serde_json::json!({ "embedding": [0.7, 0.2, -0.1] }))
                        .unwrap(),
                    delay_ms: 0,
                }
            }
        }
    }))
    .await;
    let c = cfg_for(&server.url);
    let p = OllamaEmbedder::new(&c);

    // Outage: the embed falls back and the hash vector is cached.
    let (_hash_vec, tag) = p.embed("the nightly snapshot completes at 02:00").await.unwrap();
    assert_eq!(tag, HASH_EMBED_MODEL);
    assert_eq!(p.cache_len(), 1);

    // A plain cache-checking call still surfaces the stale entry (by design;
    // it must not fire a request either).
    let (_stale, still_hash) = p.embed("the nightly snapshot completes at 02:00").await.unwrap();
    assert_eq!(still_hash, HASH_EMBED_MODEL);
    let hits_before_probe = server.hit_count();
    assert_eq!(hits_before_probe, 1, "cache hit must not perform a request");

    // Probe: the service has recovered.
    assert!(p.probe_semantic().await);
    let hits_after_probe = server.hit_count();

    // The fresh path must ignore the poisoned cache entry and issue a real
    // request, then report the semantic space.
    let (_fresh, tag_fresh) = p.embed_fresh("the nightly snapshot completes at 02:00").await.unwrap();
    assert_eq!(tag_fresh, c.embedding_model);
    assert_eq!(
        server.hit_count(),
        hits_after_probe + 1,
        "embed_fresh must issue a real request"
    );

    // The corrected result is written back: a plain lookup now reports
    // semantic without another request.
    let (_now, tag_now) = p.embed("the nightly snapshot completes at 02:00").await.unwrap();
    assert_eq!(tag_now, c.embedding_model);
    assert_eq!(
        server.hit_count(),
        hits_after_probe + 1,
        "cache must hold the corrected vector"
    );
}

// ---------------------------------------------------------------------------
// Model-tag filtering: the cross-space regression guard (plan §3.3, §13.6 —
// a build-failing invariant) and the reembed_stale candidate query.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hash_vector_never_cosine_compares_against_semantic_query() {
    let index = MemoryVectorIndex::new();
    let semantic_tag = DEFAULT_EMBEDDING_MODEL;

    // The hash-space row is the query itself (cosine 1.0 if it were ever
    // compared across spaces); the semantic row is far away. Without the tag
    // filter the hash row would win; with it, only the semantic row may
    // surface.
    let query = hash_embed("the payment gateway retries three times", 8);
    let far: Vec<f32> = (0..8).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();

    index
        .upsert("c_hash", &query, HASH_EMBED_MODEL)
        .await
        .unwrap();
    index.upsert("c_sem", &far, semantic_tag).await.unwrap();

    let hits = index.search(&query, semantic_tag, 10).await.unwrap();
    assert_eq!(hits.len(), 1, "cross-space rows must be filtered out");
    assert_eq!(hits[0].0, "c_sem");

    // And it is reachable in its own space.
    let hash_hits = index.search(&query, HASH_EMBED_MODEL, 10).await.unwrap();
    assert_eq!(hash_hits.len(), 1);
    assert_eq!(hash_hits[0].0, "c_hash");
    assert!((hash_hits[0].1 - 1.0).abs() < 1e-6);
}

#[tokio::test]
async fn reembed_candidate_query_selects_only_tagged_rows() {
    let index = MemoryVectorIndex::new();
    let semantic_tag = DEFAULT_EMBEDDING_MODEL;
    let a = hash_embed("stale fallback row one", 8);
    let b = hash_embed("stale fallback row two", 8);
    let s = hash_embed("genuine semantic row", 8);
    index.upsert("stale_1", &a, HASH_EMBED_MODEL).await.unwrap();
    index.upsert("stale_2", &b, HASH_EMBED_MODEL).await.unwrap();
    index.upsert("semantic_1", &s, semantic_tag).await.unwrap();

    let stale = index.list_by_model(HASH_EMBED_MODEL).await.unwrap();
    assert_eq!(stale, vec!["stale_1".to_string(), "stale_2".to_string()]);

    // Removing a re-embedded row drops it from the candidate set.
    index.remove("stale_1").await.unwrap();
    let stale = index.list_by_model(HASH_EMBED_MODEL).await.unwrap();
    assert_eq!(stale, vec!["stale_2".to_string()]);
}
