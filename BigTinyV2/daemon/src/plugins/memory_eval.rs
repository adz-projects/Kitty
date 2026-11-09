//! Recall evals for the two memory engines, run against the real LiteRT
//! EmbeddingGemma model (relevance thresholds mean nothing in the lexical-hash
//! space the crates' own tests use). Measurements, not gates: each run prints
//! a per-case table and summary rates, and fails only on setup errors -- or on
//! any miss when `MEMORY_EVAL_STRICT=1`.
//!
//! The labelled cases are data: `docs/memory-eval/pathway_cases.json` and
//! `docs/memory-eval/memorabilia_cases.json` (override with
//! `MEMORY_EVAL_PATHWAY` / `MEMORY_EVAL_MEMORABILIA`). Memorabilia documents
//! carry scripted claims, so extraction quality is out of scope here; this
//! measures what recall injects.
//!
//! ```text
//! KITTY_EMBED_MODEL=<embeddinggemma-2-text-270m.litertlm> \
//! KITTY_LITERT_LM_EMBED_LIB=<litert-lm-embed.dll> \
//!   cargo test --release --features litert-embed --lib -- --ignored memory_eval --nocapture
//! ```
//!
//! A legacy `.tflite` model takes `KITTY_LITERT_LIB` (`libLiteRt.dll`) and
//! `KITTY_EMBED_TOKENIZER` (`tokenizer.json`) instead of the LiteRT-LM library.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use adaptive_pathway::embed::SemanticEmbedder;

/// A fixed "now" for extraction bookkeeping, so runs are comparable.
const NOW: &str = "2026-11-09T12:00:00Z";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn fixture(var: &str, file: &str) -> String {
    let path = env(var).unwrap_or_else(|| {
        format!("{}/../../docs/memory-eval/{file}", env!("CARGO_MANIFEST_DIR"))
    });
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

/// The live embedder plus the space tag production uses for it, or `None`
/// (with a note) when the model paths aren't set.
fn live_embedder() -> Option<(Arc<dyn SemanticEmbedder>, String)> {
    let Some(model) = env("KITTY_EMBED_MODEL") else {
        eprintln!("memory_eval: set KITTY_EMBED_MODEL (and its runtime, see the module docs); skipping");
        return None;
    };
    let stem = std::path::Path::new(&model)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into());
    // Same dispatch as production (`lib.rs`): a `.litertlm` bundle runs
    // through LiteRT-LM's embedding API, a `.tflite` through raw TFLite.
    let embedder: Arc<dyn SemanticEmbedder> = if model.to_ascii_lowercase().ends_with(".litertlm") {
        let Some(lib) = env("KITTY_LITERT_LM_EMBED_LIB") else {
            eprintln!("memory_eval: a .litertlm model needs KITTY_LITERT_LM_EMBED_LIB; skipping");
            return None;
        };
        Arc::new(crate::litert::LiteRtLmEmbedder::spawn(lib, model))
    } else {
        let (Some(lib), Some(tok)) = (env("KITTY_LITERT_LIB"), env("KITTY_EMBED_TOKENIZER")) else {
            eprintln!("memory_eval: a .tflite model needs KITTY_LITERT_LIB and KITTY_EMBED_TOKENIZER; skipping");
            return None;
        };
        Arc::new(crate::litert::LiteRtEmbedder::spawn(lib, model, tok))
    };
    Some((embedder, format!("litert:{stem}")))
}

fn strict() -> bool {
    env("MEMORY_EVAL_STRICT").as_deref() == Some("1")
}

fn rate(hit: usize, total: usize) -> String {
    if total == 0 {
        "n/a".into()
    } else {
        format!("{hit}/{total} ({:.0}%)", 100.0 * hit as f64 / total as f64)
    }
}

// ---------------------------------------------------------------- pathway --

#[derive(Deserialize)]
struct PathwayFixture {
    beliefs: Vec<FixtureBelief>,
    cases: Vec<PathwayCase>,
}

#[derive(Deserialize)]
struct FixtureBelief {
    id: String,
    text: String,
    layer: String,
    provenance: String,
}

#[derive(Deserialize)]
struct PathwayCase {
    id: String,
    message: String,
    #[serde(default)]
    expect: Vec<String>,
    #[serde(default)]
    forbid: Vec<String>,
    #[serde(default)]
    expect_sections: Vec<String>,
}

fn layer(s: &str) -> adaptive_pathway::store::beliefs::Layer {
    use adaptive_pathway::store::beliefs::Layer;
    match s {
        "identity" => Layer::Identity,
        "conversation" => Layer::Conversation,
        _ => Layer::Context,
    }
}

#[tokio::test]
#[ignore]
async fn memory_eval_pathway() {
    use adaptive_pathway::config::Config;
    use adaptive_pathway::engine::PathwayEngine;
    use adaptive_pathway::store::beliefs::{Belief, Layer, Provenance};

    let Some((embedder, space)) = live_embedder() else { return };
    let fx: PathwayFixture =
        serde_json::from_str(&fixture("MEMORY_EVAL_PATHWAY", "pathway_cases.json")).expect("fixture");

    let mut cfg = Config::default();
    cfg.embedding.ollama_model = space.clone();

    // Embed the store once through the production provider (same projection
    // as a live engine), then give every case a fresh engine so no case's
    // recall state (relevance-floor EWMA, streaks) leaks into the next.
    let probe = PathwayEngine::open_with_embedder("sqlite::memory:", cfg.clone(), Some(embedder.clone()))
        .await
        .expect("probe engine");
    let mut vectors: HashMap<String, Vec<f32>> = HashMap::new();
    for b in &fx.beliefs {
        let (v, semantic) = probe.embed.embed_with_space(&b.text).await;
        assert!(semantic, "the LiteRT embedder did not answer for {:?}; check the model paths", b.id);
        vectors.insert(b.id.clone(), v);
    }

    let (mut exp_hit, mut exp_total, mut leak, mut forbid_total) = (0, 0, 0, 0);
    let (mut sec_hit, mut sec_total, mut clean_cases) = (0, 0, 0);
    let mut injected_total = 0usize;
    println!("\n{:<22} {:>3}  {:<40} sections", "case", "n", "missed / leaked");
    for case in &fx.cases {
        let engine = PathwayEngine::open_with_embedder("sqlite::memory:", cfg.clone(), Some(embedder.clone()))
            .await
            .expect("engine");
        let now = chrono::Utc::now();
        for b in &fx.beliefs {
            let provenance = Provenance::parse(&b.provenance);
            let l = layer(&b.layer);
            let (support, sessions) = match (l, provenance) {
                (Layer::Identity, _) => (3, 2),
                (_, Provenance::SingleObservation) => (1, 1),
                _ => (2, 1),
            };
            engine
                .db
                .insert_belief(&Belief {
                    id: b.id.clone(),
                    text: b.text.clone(),
                    embedding: vectors[&b.id].clone(),
                    confidence: provenance.initial_confidence(),
                    provenance,
                    layer: l,
                    tested: provenance.is_tested(),
                    domain: None,
                    tier: l.as_str().into(),
                    support_count: support,
                    distinct_sessions: sessions,
                    contradict_count: 0,
                    pinned: false,
                    last_confirmed_at: Some(now),
                    consolidated_at: None,
                    created_at: now,
                    updated_at: now,
                    session_id: None,
                    embedding_model: space.clone(),
                })
                .await
                .expect("insert belief");
        }
        let session = format!("eval-{}", case.id);
        let block = engine.recall(&session, &case.message).await.unwrap_or_default();
        let injected: HashSet<String> = engine
            .db
            .get_state(&session)
            .await
            .ok()
            .flatten()
            .map(|s| s.last_recall_ids.into_iter().collect())
            .unwrap_or_default();
        injected_total += injected.len();

        let missed: Vec<&str> =
            case.expect.iter().filter(|id| !injected.contains(*id)).map(String::as_str).collect();
        let leaked: Vec<&str> =
            case.forbid.iter().filter(|id| injected.contains(*id)).map(String::as_str).collect();
        let sections_missing: Vec<&str> = case
            .expect_sections
            .iter()
            .filter(|s| !block.contains(s.as_str()))
            .map(String::as_str)
            .collect();
        exp_total += case.expect.len();
        exp_hit += case.expect.len() - missed.len();
        forbid_total += case.forbid.len();
        leak += leaked.len();
        sec_total += case.expect_sections.len();
        sec_hit += case.expect_sections.len() - sections_missing.len();
        if missed.is_empty() && leaked.is_empty() && sections_missing.is_empty() {
            clean_cases += 1;
        }
        println!(
            "{:<22} {:>3}  {:<40} {}",
            case.id,
            injected.len(),
            format!(
                "{}{}",
                if missed.is_empty() { String::new() } else { format!("-{} ", missed.join(",")) },
                if leaked.is_empty() { String::new() } else { format!("+{}", leaked.join(",")) },
            ),
            if sections_missing.is_empty() { "ok".into() } else { format!("missing {}", sections_missing.join(", ")) },
        );
    }
    println!("\npathway recall over {} cases:", fx.cases.len());
    println!("  expected beliefs injected : {}", rate(exp_hit, exp_total));
    println!("  forbidden beliefs leaked  : {}", rate(leak, forbid_total));
    println!("  expected sections present : {}", rate(sec_hit, sec_total));
    println!("  mean beliefs injected     : {:.1}", injected_total as f64 / fx.cases.len().max(1) as f64);
    println!("  fully clean cases         : {}", rate(clean_cases, fx.cases.len()));
    if strict() {
        assert_eq!(clean_cases, fx.cases.len(), "MEMORY_EVAL_STRICT: not every case is clean");
    }
}

// ------------------------------------------------------------ memorabilia --

#[derive(Deserialize)]
struct MemFixture {
    documents: Vec<MemDocument>,
    cases: Vec<MemCase>,
}

#[derive(Deserialize)]
struct MemDocument {
    source_entity: String,
    source_name: String,
    source_type: String,
    captured_at: String,
    #[serde(default)]
    decay_class: Option<String>,
    text: String,
    claims: Vec<String>,
}

#[derive(Deserialize)]
struct MemCase {
    id: String,
    kind: String,
    message: String,
    #[serde(default)]
    expect: Vec<String>,
    #[serde(default)]
    expect_indirect: Vec<String>,
    #[serde(default)]
    forbid: Vec<String>,
}

/// Answers each extraction call with the claims scripted for the chunk's
/// source, so the eval measures recall rather than an extraction model.
struct ScriptedChat {
    by_source: HashMap<String, Value>,
}

#[async_trait]
impl memorabilia::traits::StructuredChat for ScriptedChat {
    async fn structured_chat(&self, messages: Vec<Value>, _schema: &Value) -> Result<Value, String> {
        let user = messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .find(|c| c.starts_with("CHUNK (source_entity: "))
            .ok_or("no chunk in the extraction prompt")?;
        let source = user
            .trim_start_matches("CHUNK (source_entity: ")
            .split(')')
            .next()
            .unwrap_or_default();
        self.by_source
            .get(source)
            .cloned()
            .ok_or_else(|| format!("no scripted claims for source {source}"))
    }
}

/// The production adapter's shape (`memorabilia_host::SharedSemanticEmbedder`),
/// minus the hash fallback: an eval run with the embedder down is a setup
/// error, not a data point.
struct LiveEmbedder {
    inner: Arc<dyn SemanticEmbedder>,
    tag: String,
}

#[async_trait]
impl memorabilia::traits::Embedder for LiveEmbedder {
    async fn embed(&self, text: &str) -> Result<(Vec<f32>, String), String> {
        self.inner
            .embed(text)
            .await
            .map(|v| (v, self.tag.clone()))
            .ok_or_else(|| "LiteRT embedder did not answer".to_string())
    }

    fn model_tag(&self) -> &str {
        &self.tag
    }
}

#[tokio::test]
#[ignore]
async fn memory_eval_memorabilia() {
    use memorabilia::engine::Engine;
    use memorabilia::learn::IngestInput;

    let Some((embedder, space)) = live_embedder() else { return };
    let fx: MemFixture =
        serde_json::from_str(&fixture("MEMORY_EVAL_MEMORABILIA", "memorabilia_cases.json")).expect("fixture");
    let dim = embedder
        .embed("dimension probe")
        .await
        .expect("the LiteRT embedder did not answer; check the model paths")
        .len();
    let by_source: HashMap<String, Value> = fx
        .documents
        .iter()
        .map(|d| {
            let decay = d.decay_class.clone().unwrap_or_else(|| "static".into());
            let props: Vec<Value> = d
                .claims
                .iter()
                .map(|c| {
                    json!({"claim": c, "importance": "medium", "urgency": "low",
                           "decay_class": decay, "urgency_expires_at": ""})
                })
                .collect();
            (d.source_entity.clone(), json!({"propositions": props, "disputes": []}))
        })
        .collect();
    let chat = Arc::new(ScriptedChat { by_source });
    let mem_embedder = Arc::new(LiveEmbedder { inner: embedder.clone(), tag: space.clone() });
    let config = memorabilia::config::Config {
        embedding_dim: dim,
        embedding_model: space.clone(),
        ..Default::default()
    };

    #[derive(Default)]
    struct Tally {
        hit: usize,
        total: usize,
        leak: usize,
        forbid: usize,
        clean: usize,
        cases: usize,
    }
    let mut by_kind: std::collections::BTreeMap<String, Tally> = Default::default();
    let (mut ind_hit, mut ind_total) = (0, 0);
    println!("\n{:<20} {:<10} {:>3}  missed / leaked", "case", "kind", "n");
    for case in &fx.cases {
        // A fresh store per case: recall's relevance-floor EWMA would
        // otherwise carry one case's cut into the next.
        let engine = Engine::open_at("sqlite::memory:", config.clone(), chat.clone(), mem_embedder.clone())
            .await
            .expect("engine");
        for d in &fx.documents {
            engine
                .ingest(&IngestInput {
                    content: d.text.clone(),
                    source_type: d.source_type.clone(),
                    source_name: d.source_name.clone(),
                    source_entity: d.source_entity.clone(),
                    captured_at: d.captured_at.clone(),
                    intent: None,
                })
                .await;
        }
        for _ in 0..50 {
            let o = engine.drain_extraction(NOW).await;
            assert_eq!(o.failed, 0, "scripted extraction failed");
            if o.attempted == 0 {
                break;
            }
        }

        let block = engine.recall(&case.message).await.unwrap_or_default();
        let lines: Vec<&str> = block.lines().filter(|l| l.starts_with("- [")).collect();
        let present = |needle: &str| lines.iter().any(|l| l.contains(needle));
        let missed: Vec<&str> = case.expect.iter().filter(|s| !present(s)).map(String::as_str).collect();
        let leaked: Vec<&str> = case.forbid.iter().filter(|s| present(s)).map(String::as_str).collect();
        let ind_missed: Vec<&str> =
            case.expect_indirect.iter().filter(|s| !present(s)).map(String::as_str).collect();
        ind_total += case.expect_indirect.len();
        ind_hit += case.expect_indirect.len() - ind_missed.len();

        let t = by_kind.entry(case.kind.clone()).or_default();
        t.cases += 1;
        t.total += case.expect.len();
        t.hit += case.expect.len() - missed.len();
        t.forbid += case.forbid.len();
        t.leak += leaked.len();
        if missed.is_empty() && leaked.is_empty() && ind_missed.is_empty() {
            t.clean += 1;
        }
        let mut notes = Vec::new();
        if !missed.is_empty() {
            notes.push(format!("-[{}]", missed.join("; ")));
        }
        if !ind_missed.is_empty() {
            notes.push(format!("-indirect[{}]", ind_missed.join("; ")));
        }
        if !leaked.is_empty() {
            notes.push(format!("+[{}]", leaked.join("; ")));
        }
        println!(
            "{:<20} {:<10} {:>3}  {}",
            case.id,
            case.kind,
            lines.len(),
            if notes.is_empty() { "ok".into() } else { notes.join(" ") }
        );
    }
    println!("\nmemorabilia recall over {} cases:", fx.cases.len());
    let mut all_clean = true;
    for (kind, t) in &by_kind {
        all_clean &= t.clean == t.cases;
        println!(
            "  {kind:<10} expected {:<14} leaked {:<14} clean {}",
            rate(t.hit, t.total),
            rate(t.leak, t.forbid),
            rate(t.clean, t.cases)
        );
    }
    println!("  indirect targets reached : {}", rate(ind_hit, ind_total));
    if strict() {
        assert!(all_clean, "MEMORY_EVAL_STRICT: not every case is clean");
    }
}

/// Sanity check for the evals above: does the embedder itself rank the
/// obviously-right belief first by raw cosine? If it does and recall still
/// misses, the loss is in recall's selection, not in the vectors.
#[tokio::test]
#[ignore]
async fn memory_eval_embedding_sanity() {
    let Some((embedder, _)) = live_embedder() else { return };
    let fx: PathwayFixture =
        serde_json::from_str(&fixture("MEMORY_EVAL_PATHWAY", "pathway_cases.json")).expect("fixture");
    let mut vecs = Vec::new();
    for b in &fx.beliefs {
        vecs.push((b.id.clone(), embedder.embed(&b.text).await.expect("embed")));
    }
    let cos = |a: &[f32], b: &[f32]| {
        let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        d / (na * nb)
    };
    let (mut top1, mut top3, mut n) = (0, 0, 0);
    for case in &fx.cases {
        let q = embedder.embed(&case.message).await.expect("embed");
        let mut ranked: Vec<(f32, &str)> = vecs.iter().map(|(id, v)| (cos(&q, v), id.as_str())).collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        let top: Vec<String> = ranked.iter().take(4).map(|(c, id)| format!("{id}:{c:.3}")).collect();
        println!("{:<22} {}", case.id, top.join("  "));
        if let Some(first) = case.expect.first() {
            n += 1;
            if ranked[0].1 == first {
                top1 += 1;
            }
            if ranked.iter().take(3).any(|(_, id)| id == first) {
                top3 += 1;
            }
        }
    }
    println!("first expected belief ranked #1: {top1}/{n}, in top 3: {top3}/{n}");
}

// ------------------------------------------------------- relevance floor --

/// One (question, memory item) pair the floor has to judge.
struct FloorPair {
    raw: f32,
    centered: f32,
    /// `Some(true)` expected, `Some(false)` forbidden, `None` unlabelled.
    label: Option<bool>,
    /// The question should inject nothing at all (a `negative` case).
    off_topic: bool,
    case: usize,
}

fn dot32(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn cos32(a: &[f32], b: &[f32]) -> f32 {
    let (na, nb) = (dot32(a, a).sqrt(), dot32(b, b).sqrt());
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot32(a, b) / (na * nb)
    }
}

fn mean_vec(vs: &[Vec<f32>]) -> Vec<f32> {
    let mut m = vec![0.0f32; vs[0].len()];
    for v in vs {
        for (s, x) in m.iter_mut().zip(v) {
            *s += x / vs.len() as f32;
        }
    }
    m
}

fn centered(q: &[f32], v: &[f32], mean: &[f32]) -> f32 {
    let qc: Vec<f32> = q.iter().zip(mean).map(|(a, b)| a - b).collect();
    let vc: Vec<f32> = v.iter().zip(mean).map(|(a, b)| a - b).collect();
    cos32(&qc, &vc)
}

/// Sweep candidate floors over both scores and print, per threshold: the
/// share of expected items that pass, the share of forbidden items that leak,
/// and how many off-topic questions inject nothing. Also prints the floor that
/// maximises `expected_pass - 2 * forbidden_leak` (leaks weighted double:
/// "err on the side of too selective", docs/MEMORY_OVERNIGHT.md).
fn sweep(label: &str, pairs: &[FloorPair], n_cases: usize) {
    let lo_hi = |f: fn(&FloorPair) -> f32| {
        let vals: Vec<f32> = pairs.iter().map(f).collect();
        let lo = vals.iter().cloned().fold(f32::INFINITY, f32::min);
        let hi = vals.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        (lo, hi)
    };
    for (score_name, get) in [
        ("raw cosine", (|p: &FloorPair| p.raw) as fn(&FloorPair) -> f32),
        ("centered cosine", |p: &FloorPair| p.centered),
    ] {
        let (lo, hi) = lo_hi(get);
        let exp_total = pairs.iter().filter(|p| p.label == Some(true)).count().max(1);
        let forb_total = pairs.iter().filter(|p| p.label == Some(false)).count().max(1);
        let off_cases: std::collections::BTreeSet<usize> =
            pairs.iter().filter(|p| p.off_topic).map(|p| p.case).collect();
        println!("\n[{label}] {score_name}: range {lo:.3}..{hi:.3}  ({n_cases} cases, {} off-topic)", off_cases.len());
        println!("  floor   expected-pass  forbidden-leak  off-topic-clean  score");
        let mut best = (f32::NEG_INFINITY, 0.0f32);
        let steps = 40;
        for i in 0..=steps {
            let t = lo + (hi - lo) * i as f32 / steps as f32;
            let pass = pairs.iter().filter(|p| p.label == Some(true) && get(p) >= t).count();
            let leak = pairs.iter().filter(|p| p.label == Some(false) && get(p) >= t).count();
            let dirty: std::collections::BTreeSet<usize> =
                pairs.iter().filter(|p| p.off_topic && get(p) >= t).map(|p| p.case).collect();
            let (ep, fl) = (pass as f32 / exp_total as f32, leak as f32 / forb_total as f32);
            let score = ep - 2.0 * fl;
            if score > best.0 {
                best = (score, t);
            }
            if i % 2 == 0 {
                println!(
                    "  {t:>6.3}  {:>5}/{:<5} {:>3.0}%  {:>4}/{:<4} {:>3.0}%  {:>3}/{:<3}         {score:+.2}",
                    pass, exp_total, ep * 100.0, leak, forb_total, fl * 100.0,
                    off_cases.len() - dirty.len(), off_cases.len()
                );
            }
        }
        println!("  best floor ({score_name}): {:.3}  (score {:+.2})", best.1, best.0);
    }
}

#[tokio::test]
#[ignore]
async fn memory_eval_relevance_floor() {
    let Some((embedder, _)) = live_embedder() else { return };

    // Pathway: every non-identity belief against every case. Identity beliefs
    // use their own, gentler floor and are never forbidden.
    let fx: PathwayFixture =
        serde_json::from_str(&fixture("MEMORY_EVAL_PATHWAY", "pathway_cases.json")).expect("fixture");
    let mut beliefs = Vec::new();
    for b in fx.beliefs.iter().filter(|b| b.layer != "identity") {
        beliefs.push((b.id.clone(), embedder.embed(&b.text).await.expect("embed")));
    }
    let mean = mean_vec(&beliefs.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>());
    let mut pairs = Vec::new();
    for (ci, case) in fx.cases.iter().enumerate() {
        let q = embedder.embed(&case.message).await.expect("embed");
        let off_topic = case.expect.is_empty();
        for (id, v) in &beliefs {
            let label = if case.expect.contains(id) {
                Some(true)
            } else if case.forbid.contains(id) {
                Some(false)
            } else {
                None
            };
            pairs.push(FloorPair { raw: cos32(&q, v), centered: centered(&q, v, &mean), label, off_topic, case: ci });
        }
    }
    sweep("pathway", &pairs, fx.cases.len());

    // Memorabilia: each document is one chunk; a document is expected when it
    // holds an expected claim (direct cases only -- indirect targets are not
    // a floor's job), forbidden when it holds a forbidden one.
    let mx: MemFixture =
        serde_json::from_str(&fixture("MEMORY_EVAL_MEMORABILIA", "memorabilia_cases.json")).expect("fixture");
    let mut docs = Vec::new();
    for d in &mx.documents {
        docs.push((d, embedder.embed(&d.text).await.expect("embed")));
    }
    let mean = mean_vec(&docs.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>());
    let holds = |d: &MemDocument, needles: &[String]| needles.iter().any(|n| d.claims.iter().any(|c| c.contains(n.as_str())));
    let mut pairs = Vec::new();
    for (ci, case) in mx.cases.iter().enumerate() {
        let q = embedder.embed(&case.message).await.expect("embed");
        let off_topic = case.kind == "negative";
        for (d, v) in &docs {
            let label = if holds(d, &case.expect) {
                Some(true)
            } else if holds(d, &case.forbid) {
                Some(false)
            } else {
                None
            };
            pairs.push(FloorPair { raw: cos32(&q, v), centered: centered(&q, v, &mean), label, off_topic, case: ci });
        }
    }
    sweep("memorabilia", &pairs, mx.cases.len());
}
