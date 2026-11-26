//! Minimal Windows-"Run"-style overlay runs: background submission, zero-loss
//! event buffering until a full Kitty window opens, reveal triggers (first
//! visible output, HITL pause, or error/completion), and semantic rapid-repeat
//! prompt deduplication.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{ImageAttachment, ThinkingEffort};
use crate::notifications;
use crate::state::AppState;

/// How long a submitted overlay prompt stays eligible for rapid-repeat
/// deduplication (longer while the corresponding turn is still in flight).
const DEDUP_WINDOW: Duration = Duration::from_secs(60);

/// Cap on remembered overlay submissions so memory stays bounded.
const MAX_RECENT_PROMPTS: usize = 64;

/// Short timeout for BigTiny's local loopback `/api/embeddings` call during
/// dedup checks so an unresponsive embedder never stalls session creation.
const EMBED_TIMEOUT: Duration = Duration::from_millis(500);

/// Safety TTL for un-drained overlay run buffers when no window opened.
const UNOPENED_BUFFER_TTL: Duration = Duration::from_secs(60);

/// Longer safety TTL once a full Kitty window has been opened, giving slow
/// cold-start webviews plenty of time to mount and drain before falling back
/// to persisted session replay.
const OPENED_BUFFER_TTL: Duration = Duration::from_secs(300);

/// Payload sent by the minimal Concept D overlay on Submit.
#[derive(Debug, Clone, Deserialize)]
pub struct OverlayPromptRequest {
    pub text: String,
    #[serde(default)]
    pub images: Option<Vec<ImageAttachment>>,
    #[serde(default)]
    pub attached_paths: Option<Vec<String>>,
    #[serde(default)]
    pub documents: Option<Vec<crate::bigtiny::turn_text::InlineDocument>>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub thinking_effort: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub debate: Option<bool>,
    #[serde(default)]
    pub research: Option<crate::bigtiny::research::ResearchArgs>,
    #[serde(default)]
    pub critique: Option<String>,
    #[serde(default)]
    pub tidy: Option<bool>,
    #[serde(default)]
    pub raw_prompt: Option<String>,
}

/// One-shot drain response consumed by the newly opened full Kitty window
/// (`adoptOverlayRun`) so buffered pre-reveal events and live events replay
/// in exact order without gaps or duplicates.
#[derive(Debug, Clone, Serialize)]
pub struct OverlayDrainResult {
    pub events: Vec<Value>,
    pub busy: bool,
}

static RUN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Live state for a chat started from the minimal overlay.
pub struct OverlayRunState {
    pub seq: u64,
    pub submitted_at: Instant,
    pub session_id: String,
    pub cwd: String,
    pub prompt_preview: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub thinking_effort: Option<ThinkingEffort>,
    pub is_default_folder: bool,
    pub window_opened: bool,
    pub handed_off: bool,
    pub buffered_events: Vec<Value>,
    pub in_leaked_think: bool,
}

/// Recent overlay submission kept for rapid-repeat semantic deduplication.
pub struct RecentOverlayPrompt {
    pub id: u64,
    pub raw_text: String,
    pub normalized_text: String,
    pub core_text: String,
    pub attachment_sig: String,
    pub submitted_at: Instant,
    pub session_id: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub core_embedding: Option<Vec<f32>>,
}

#[derive(Clone)]
enum RevealReason {
    Output,
    Complete,
    Error(String),
    #[cfg_attr(not(desktop), allow(dead_code))]
    Hitl {
        tool_name: String,
        scheduled: bool,
    },
}

// ---------------------------------------------------------------------------
// Prompt preview & attachment signature
// ---------------------------------------------------------------------------

fn make_prompt_preview(text: &str, has_attachments: bool) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return if has_attachments {
            "(attachment)".to_string()
        } else {
            "New prompt".to_string()
        };
    }
    const MAX_CHARS: usize = 72;
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn build_attachment_signature(req: &OverlayPromptRequest) -> String {
    let cwd_sig = req
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.replace('\\', "/"))
        .unwrap_or_default();
    let mut paths = req.attached_paths.clone().unwrap_or_default();
    paths.sort();
    let mut doc_labels: Vec<String> = req
        .documents
        .as_ref()
        .map(|docs| docs.iter().map(|d| d.label.clone()).collect())
        .unwrap_or_default();
    doc_labels.sort();
    let image_count = req.images.as_ref().map(|i| i.len()).unwrap_or(0);
    let slash_sig = format!(
        "db:{},rs:{},cr:{},td:{}",
        req.debate.unwrap_or(false),
        req.research
            .as_ref()
            .map(|r| r.max_turns.map(|t| t.to_string()).unwrap_or_else(|| "default".into()))
            .unwrap_or_default(),
        req.critique.as_deref().unwrap_or(""),
        req.tidy.unwrap_or(false),
    );
    format!(
        "cwd:{cwd_sig}|slash:{slash_sig}|p:[{}]|d:[{}]|img:{image_count}",
        paths.join(","),
        doc_labels.join(",")
    )
}

// ---------------------------------------------------------------------------
// Semantic deduplication helpers (pure + embedding client)
// ---------------------------------------------------------------------------

pub(crate) fn normalize_prompt(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Multi-word lead-in phrases stripped iteratively from the start of a
/// normalized prompt so conversational request wrappers don't inflate
/// similarity across unrelated topics.
const LEAD_IN_PHRASES: &[&[&str]] = &[
    &["hey", "kitty"],
    &["hi", "kitty"],
    &["can", "you"],
    &["could", "you"],
    &["would", "you"],
    &["will", "you"],
    &["i", "need"],
    &["i", "want"],
    &["i", "would", "like"],
    &["help", "me"],
    &["look", "up"],
    &["search", "for"],
    &["what", "are"],
    &["what", "is"],
];

const LEAD_IN_WORDS: &[&str] = &[
    "kitty",
    "please",
    "find",
    "get",
    "give",
    "show",
    "tell",
    "search",
    "me",
    "some",
    "any",
    "information",
    "info",
    "details",
    "about",
    "for",
    "on",
];

fn strip_boilerplate_words<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    while words.len() > 1 {
        let mut matched = false;
        for phrase in LEAD_IN_PHRASES {
            if words.len() > phrase.len() && &words[..phrase.len()] == *phrase {
                words = &words[phrase.len()..];
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if LEAD_IN_WORDS.contains(&words[0]) {
            words = &words[1..];
            continue;
        }
        break;
    }
    words
}

fn strip_boilerplate_norm(norm: &str) -> String {
    let words: Vec<&str> = norm.split_whitespace().collect();
    strip_boilerplate_words(&words).join(" ")
}

fn strip_shared_prefix_norm(norm_a: &str, norm_b: &str) -> (String, String) {
    let a_raw: Vec<&str> = norm_a.split_whitespace().collect();
    let b_raw: Vec<&str> = norm_b.split_whitespace().collect();
    let a_words = strip_boilerplate_words(&a_raw);
    let b_words = strip_boilerplate_words(&b_raw);

    let mut common = 0usize;
    while common < a_words.len()
        && common < b_words.len()
        && a_words[common] == b_words[common]
    {
        common += 1;
    }
    if common > 0 && common < a_words.len() && common < b_words.len() {
        (a_words[common..].join(" "), b_words[common..].join(" "))
    } else {
        (a_words.join(" "), b_words.join(" "))
    }
}

/// Strip common request boilerplate and any shared leading prefix words when
/// comparing two prompts so shared templates ("find me information about ...")
/// don't inflate similarity between unrelated subjects.
#[cfg(test)]
pub(crate) fn strip_shared_prefix_and_filler(a: &str, b: &str) -> (String, String) {
    let norm_a = normalize_prompt(a);
    let norm_b = normalize_prompt(b);
    strip_shared_prefix_norm(&norm_a, &norm_b)
}

pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let fx = f64::from(*x);
        let fy = f64::from(*y);
        dot += fx * fy;
        na += fx * fx;
        nb += fy * fy;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        (dot / denom) as f32
    }
}

pub(crate) fn embeddings_match(full_sim: f32, core_sim: Option<f32>) -> bool {
    full_sim >= 0.88 || (full_sim >= 0.78 && core_sim.unwrap_or(full_sim) >= 0.72)
}

/// Word-boundary phrase aliases for the offline lexical fallback (used only
/// when BigTiny's neural `/api/embeddings` endpoint is unavailable).
const PHRASE_ALIASES: &[(&[&str], &str)] = &[
    (&["los", "angeles"], "la"),
    (&["new", "york", "city"], "nyc"),
    (&["new", "york"], "nyc"),
    (&["san", "francisco"], "sf"),
    (&["washington", "d", "c"], "dc"),
    (&["washington", "dc"], "dc"),
    (&["united", "states"], "usa"),
    (&["united", "kingdom"], "uk"),
    (&["flower", "shops"], "florist"),
    (&["flower", "shop"], "florist"),
    (&["coffee", "shops"], "cafe"),
    (&["coffee", "shop"], "cafe"),
    (&["barber", "shops"], "barbershop"),
    (&["barber", "shop"], "barbershop"),
];

const LEXICAL_STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "in",
    "on",
    "at",
    "for",
    "of",
    "to",
    "me",
    "my",
    "find",
    "information",
    "info",
    "details",
    "about",
    "around",
    "near",
    "please",
    "can",
    "you",
    "could",
    "would",
    "tell",
    "give",
    "get",
    "show",
    "search",
    "look",
    "up",
    "some",
    "any",
    "best",
    "good",
];

fn singularize_token(tok: &str) -> String {
    if tok.len() > 4 && tok.ends_with("ies") {
        format!("{}y", &tok[..tok.len() - 3])
    } else if tok.len() > 4 && tok.ends_with("es")
        && (tok.ends_with("ches")
            || tok.ends_with("shes")
            || tok.ends_with("xes")
            || tok.ends_with("sses"))
    {
        tok[..tok.len() - 2].to_string()
    } else if tok.len() > 3 && tok.ends_with('s') && !tok.ends_with("ss") && !tok.ends_with("us") {
        tok[..tok.len() - 1].to_string()
    } else {
        tok.to_string()
    }
}

/// Tokenize an already-normalized string, applying word-boundary phrase
/// aliases, stop-word removal, and light singularization in a single pass.
fn canonical_tokens_from_norm(norm: &str) -> Vec<String> {
    let raw: Vec<&str> = norm.split_whitespace().collect();
    let mut aliased: Vec<&str> = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        let mut replaced = None;
        for (seq, target) in PHRASE_ALIASES {
            if i + seq.len() <= raw.len() && &raw[i..i + seq.len()] == *seq {
                replaced = Some((*target, seq.len()));
                break;
            }
        }
        if let Some((target, step)) = replaced {
            aliased.push(target);
            i += step;
        } else {
            aliased.push(raw[i]);
            i += 1;
        }
    }

    aliased
        .into_iter()
        .filter(|w| !LEXICAL_STOP_WORDS.contains(w))
        .map(singularize_token)
        .filter(|w| !w.is_empty())
        .collect()
}

fn char_trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        let mut set = HashSet::new();
        if !s.is_empty() {
            set.insert(s.to_string());
        }
        return set;
    }
    chars
        .windows(3)
        .map(|w| w.iter().collect::<String>())
        .collect()
}

fn lexical_fallback_match_norm(norm_a: &str, norm_b: &str) -> bool {
    if norm_a.is_empty() || norm_b.is_empty() {
        return norm_a == norm_b;
    }
    if norm_a == norm_b {
        return true;
    }
    let (core_a, core_b) = strip_shared_prefix_norm(norm_a, norm_b);
    let vec_a = canonical_tokens_from_norm(&core_a);
    let vec_b = canonical_tokens_from_norm(&core_b);
    if vec_a.is_empty() || vec_b.is_empty() {
        return false;
    }
    let tok_a: HashSet<&str> = vec_a.iter().map(String::as_str).collect();
    let tok_b: HashSet<&str> = vec_b.iter().map(String::as_str).collect();
    let tok_inter = tok_a.intersection(&tok_b).count();
    let tok_union = tok_a.union(&tok_b).count();
    let jaccard = tok_inter as f32 / tok_union as f32;
    if jaccard >= 0.75 {
        return true;
    }

    let canon_a = vec_a.join(" ");
    let canon_b = vec_b.join(" ");
    if !canon_a.is_empty() && canon_a == canon_b {
        return true;
    }
    let tri_a = char_trigrams(&canon_a);
    let tri_b = char_trigrams(&canon_b);
    if tri_a.is_empty() || tri_b.is_empty() {
        return false;
    }
    let tri_inter = tri_a.intersection(&tri_b).count();
    let dice = (2 * tri_inter) as f32 / (tri_a.len() + tri_b.len()) as f32;
    dice >= 0.82
}

/// Offline / no-embedder lexical fallback for rapid-repeat detection.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn lexical_fallback_match(a: &str, b: &str) -> bool {
    let norm_a = normalize_prompt(a);
    let norm_b = normalize_prompt(b);
    lexical_fallback_match_norm(&norm_a, &norm_b)
}

async fn fetch_embeddings(app: &AppHandle, texts: &[String]) -> Option<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Some(Vec::new());
    }
    let client = crate::bigtiny::client::ensure_client(app).ok()?;
    let resp = client
        .request(reqwest::Method::POST, "/api/embeddings")
        .timeout(EMBED_TIMEOUT)
        .json(&json!({ "input": texts }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: Value = resp.json().await.ok()?;
    let arr = json.get("embeddings")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let vec = item
            .as_array()?
            .iter()
            .filter_map(|n| n.as_f64().map(|f| f as f32))
            .collect::<Vec<f32>>();
        if vec.is_empty() {
            return None;
        }
        out.push(vec);
    }
    (out.len() == texts.len()).then_some(out)
}

fn next_prompt_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn prune_recent_prompts(state: &AppState) {
    let in_flight = state.in_flight_sessions.lock().unwrap().clone();
    let mut recent = state.recent_overlay_prompts.lock().unwrap();
    recent.retain(|r| {
        r.submitted_at.elapsed() <= DEDUP_WINDOW
            || r.session_id
                .as_ref()
                .is_some_and(|sid| in_flight.contains(sid))
    });
    if recent.len() > MAX_RECENT_PROMPTS {
        let drop_count = recent.len() - MAX_RECENT_PROMPTS;
        recent.drain(..drop_count);
    }
}

struct DedupCandidate {
    id: u64,
    raw_text: String,
    normalized_text: String,
    core_text: String,
    embedding: Option<Vec<f32>>,
    core_embedding: Option<Vec<f32>>,
}

/// Check whether `text` + `attachment_sig` duplicates a recent or in-flight
/// overlay submission; if not, record it and return its id. Serialized via
/// `AppState::overlay_dedup_gate` so double-Enter bursts cannot race.
async fn check_and_record_prompt(
    app: &AppHandle,
    text: &str,
    attachment_sig: String,
) -> Option<u64> {
    let state = app.state::<AppState>();
    let _gate = state.overlay_dedup_gate.lock().await;

    let norm = normalize_prompt(text);
    let new_core = strip_boilerplate_norm(&norm);

    prune_recent_prompts(&state);

    let candidates: Vec<DedupCandidate> = {
        let recent = state.recent_overlay_prompts.lock().unwrap();
        // Fast path: exact match or strong lexical match returns immediately
        // without waiting on `/api/embeddings`.
        if recent.iter().any(|r| {
            r.attachment_sig == attachment_sig
                && (r.normalized_text == norm
                    || (!norm.is_empty()
                        && !r.normalized_text.is_empty()
                        && lexical_fallback_match_norm(&norm, &r.normalized_text)))
        }) {
            return None;
        }
        recent
            .iter()
            .filter(|r| {
                r.attachment_sig == attachment_sig
                    && !norm.is_empty()
                    && !r.normalized_text.is_empty()
            })
            .map(|r| DedupCandidate {
                id: r.id,
                raw_text: r.raw_text.clone(),
                normalized_text: r.normalized_text.clone(),
                core_text: r.core_text.clone(),
                embedding: r.embedding.clone(),
                core_embedding: r.core_embedding.clone(),
            })
            .collect()
    };

    if !candidates.is_empty() {
        let mut texts_to_embed: Vec<String> = vec![text.to_string()];
        let new_core_idx = if !new_core.is_empty() && new_core != norm {
            texts_to_embed.push(new_core.clone());
            Some(1usize)
        } else {
            None
        };

        let mut missing_full: Vec<(usize, usize)> = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            if c.embedding.is_none() {
                missing_full.push((i, texts_to_embed.len()));
                texts_to_embed.push(c.raw_text.clone());
            }
        }

        // For each candidate, use cached `core_embedding` when there is no
        // extra shared prefix beyond boilerplate; otherwise embed the pair's
        // shared-prefix-stripped cores.
        enum PairCorePlan {
            Empty,
            CachedOrEmbed {
                cand_core_embed_slot: Option<usize>,
                pair_slots: Option<(usize, usize)>,
            },
        }
        let mut core_plans: Vec<PairCorePlan> = Vec::with_capacity(candidates.len());
        let mut missing_cand_cores: Vec<(usize, usize)> = Vec::new();

        for (i, c) in candidates.iter().enumerate() {
            let (ca, cb) = strip_shared_prefix_norm(&norm, &c.normalized_text);
            if ca.is_empty() || cb.is_empty() {
                core_plans.push(PairCorePlan::Empty);
            } else if ca == new_core && cb == c.core_text {
                let slot = if c.core_embedding.is_none() {
                    let s = texts_to_embed.len();
                    texts_to_embed.push(c.core_text.clone());
                    missing_cand_cores.push((i, s));
                    Some(s)
                } else {
                    None
                };
                core_plans.push(PairCorePlan::CachedOrEmbed {
                    cand_core_embed_slot: slot,
                    pair_slots: None,
                });
            } else {
                let sa = texts_to_embed.len();
                texts_to_embed.push(ca);
                let sb = texts_to_embed.len();
                texts_to_embed.push(cb);
                core_plans.push(PairCorePlan::CachedOrEmbed {
                    cand_core_embed_slot: None,
                    pair_slots: Some((sa, sb)),
                });
            }
        }

        if let Some(vecs) = fetch_embeddings(app, &texts_to_embed).await {
            let new_full_vec = &vecs[0];
            let new_core_vec = new_core_idx.map(|idx| &vecs[idx]).unwrap_or(new_full_vec);

            // Backfill any newly computed candidate embeddings into cache.
            if !missing_full.is_empty() || !missing_cand_cores.is_empty() {
                let mut filled_full: HashMap<u64, Vec<f32>> = HashMap::new();
                for &(cand_idx, vec_idx) in &missing_full {
                    filled_full.insert(candidates[cand_idx].id, vecs[vec_idx].clone());
                }
                let mut filled_core: HashMap<u64, Vec<f32>> = HashMap::new();
                for &(cand_idx, vec_idx) in &missing_cand_cores {
                    filled_core.insert(candidates[cand_idx].id, vecs[vec_idx].clone());
                }
                let mut recent = state.recent_overlay_prompts.lock().unwrap();
                for r in recent.iter_mut() {
                    if let Some(e) = filled_full.remove(&r.id) {
                        r.embedding = Some(e);
                    }
                    if let Some(ce) = filled_core.remove(&r.id) {
                        r.core_embedding = Some(ce);
                    }
                }
            }

            for (i, c) in candidates.iter().enumerate() {
                let cand_full = c.embedding.as_ref().or_else(|| {
                    missing_full
                        .iter()
                        .find(|&&(idx, _)| idx == i)
                        .map(|&(_, slot)| &vecs[slot])
                });
                let Some(cand_full) = cand_full else {
                    continue;
                };
                let full_sim = cosine_similarity(new_full_vec, cand_full);
                let core_sim = match &core_plans[i] {
                    PairCorePlan::Empty => None,
                    PairCorePlan::CachedOrEmbed {
                        cand_core_embed_slot,
                        pair_slots: Some((sa, sb)),
                    } => {
                        let _ = cand_core_embed_slot;
                        Some(cosine_similarity(&vecs[*sa], &vecs[*sb]))
                    }
                    PairCorePlan::CachedOrEmbed {
                        cand_core_embed_slot,
                        pair_slots: None,
                    } => c
                        .core_embedding
                        .as_ref()
                        .or_else(|| cand_core_embed_slot.map(|s| &vecs[s]))
                        .map(|cand_core_vec| cosine_similarity(new_core_vec, cand_core_vec)),
                };
                if embeddings_match(full_sim, core_sim) {
                    return None;
                }
            }

            let id = next_prompt_id();
            state
                .recent_overlay_prompts
                .lock()
                .unwrap()
                .push(RecentOverlayPrompt {
                    id,
                    raw_text: text.to_string(),
                    normalized_text: norm,
                    core_text: new_core,
                    attachment_sig,
                    submitted_at: Instant::now(),
                    session_id: None,
                    embedding: Some(new_full_vec.clone()),
                    core_embedding: Some(new_core_vec.clone()),
                });
            return Some(id);
        }
    }

    let id = next_prompt_id();
    state
        .recent_overlay_prompts
        .lock()
        .unwrap()
        .push(RecentOverlayPrompt {
            id,
            raw_text: text.to_string(),
            normalized_text: norm.clone(),
            core_text: new_core.clone(),
            attachment_sig,
            submitted_at: Instant::now(),
            session_id: None,
            embedding: None,
            core_embedding: None,
        });

    // Warm full + core embeddings asynchronously so a fast second submit hits cache.
    if !norm.is_empty() {
        let app_bg = app.clone();
        let text_owned = text.to_string();
        let core_owned = new_core;
        let same_as_norm = core_owned.is_empty() || core_owned == norm;
        tauri::async_runtime::spawn(async move {
            let inputs = if same_as_norm {
                vec![text_owned]
            } else {
                vec![text_owned, core_owned]
            };
            if let Some(vecs) = fetch_embeddings(&app_bg, &inputs).await {
                let state = app_bg.state::<AppState>();
                let mut recent = state.recent_overlay_prompts.lock().unwrap();
                if let Some(entry) = recent.iter_mut().find(|r| r.id == id) {
                    let full = vecs[0].clone();
                    let core = if vecs.len() > 1 {
                        vecs[1].clone()
                    } else {
                        full.clone()
                    };
                    entry.embedding = Some(full);
                    entry.core_embedding = Some(core);
                }
            }
        });
    }

    Some(id)
}

fn attach_session_to_recent_prompt(app: &AppHandle, record_id: u64, session_id: &str) {
    let state = app.state::<AppState>();
    let mut recent = state.recent_overlay_prompts.lock().unwrap();
    if let Some(entry) = recent.iter_mut().find(|r| r.id == record_id) {
        entry.session_id = Some(session_id.to_string());
    }
}

fn remove_recent_prompt(app: &AppHandle, record_id: u64) {
    let state = app.state::<AppState>();
    let mut recent = state.recent_overlay_prompts.lock().unwrap();
    recent.retain(|r| r.id != record_id);
}

// ---------------------------------------------------------------------------
// Reveal triggers & zero-loss event buffering
// ---------------------------------------------------------------------------

/// Whether `event_name`/`payload` represents the start of visible output (or a
/// terminal completion/error) that should reveal the new full Kitty window.
/// Updates `in_leaked_think` when an `llm_delta` opens/closes `<think>`.
pub(crate) fn is_reveal_trigger(
    event_name: &str,
    payload: &Value,
    in_leaked_think: &mut bool,
) -> bool {
    match event_name {
        "chat://complete" | "chat://error" => true,
        "chat://message-delta" => {
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.is_empty() {
                return false;
            }
            // Scan outside `<think>...</think>` blocks in case a model leaks
            // reasoning tags through `llm_delta`.
            let mut rest = text;
            while !rest.is_empty() {
                if *in_leaked_think {
                    if let Some(end) = rest.find("</think>") {
                        *in_leaked_think = false;
                        rest = &rest[end + "</think>".len()..];
                    } else {
                        return false;
                    }
                } else if let Some(start) = rest.find("<think>") {
                    if !rest[..start].trim().is_empty() {
                        return true;
                    }
                    *in_leaked_think = true;
                    rest = &rest[start + "<think>".len()..];
                } else {
                    return !rest.trim().is_empty();
                }
            }
            false
        }
        _ => false,
    }
}

fn build_handoff_payload(run: &OverlayRunState) -> Value {
    json!({
        "session_id": run.session_id,
        "cwd": run.cwd,
        "thinking_effort": run.thinking_effort,
        "is_default_folder": run.is_default_folder,
        "provider_id": run.provider_id,
        "model_id": run.model_id,
        "overlay_run": true,
    })
}

fn handle_reveal_open_failure(app: &AppHandle, session_id: &str, reason: &RevealReason) {
    {
        let state = app.state::<AppState>();
        let mut runs = state.overlay_runs.lock().unwrap();
        if let Some(run) = runs.get_mut(session_id) {
            if !run.handed_off {
                run.window_opened = false;
            }
        }
    }
    notifications::refresh_tray(app);

    match reason {
        RevealReason::Output => {}
        RevealReason::Complete => {
            notifications::notify_if_hidden(
                app,
                notifications::Event::TaskComplete,
                "Kitty finished",
                "Your task is complete.",
                Some(session_id),
            );
        }
        RevealReason::Error(msg) => {
            notifications::notify_if_hidden(
                app,
                notifications::Event::TaskFailed,
                "Kitty ran into a problem",
                msg,
                Some(session_id),
            );
        }
        RevealReason::Hitl {
            tool_name,
            scheduled,
        } => {
            let title = if *scheduled {
                "A scheduled task needs your approval"
            } else {
                "Approval needed"
            };
            notifications::notify_if_hidden(
                app,
                notifications::Event::ApprovalNeeded,
                title,
                &format!("Kitty wants to run {tool_name}"),
                Some(session_id),
            );
        }
    }
}

fn open_revealed_window(
    app: &AppHandle,
    session_id: String,
    handoff: Value,
    reason: RevealReason,
    submitted_at: Instant,
    force_focus: bool,
) {
    let app_main = app.clone();
    let sid_main = session_id.clone();
    let reason_main = reason.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        let elapsed_ms = submitted_at.elapsed().as_millis() as u64;
        let steal_focus = crate::windows::should_steal_focus_on_reveal(
            force_focus,
            elapsed_ms,
            crate::windows::user_idle_ms(),
        );
        if let Err(err) = crate::windows::open_new_chat_window_with_focus(
            &app_main,
            Some(handoff),
            steal_focus,
        ) {
            tracing::warn!("failed to open Kitty window for overlay run: {err}");
            handle_reveal_open_failure(&app_main, &sid_main, &reason_main);
        }
    }) {
        tracing::warn!("failed to dispatch window open for overlay run: {e}");
        handle_reveal_open_failure(app, &session_id, &reason);
    }
}

/// Emit `event_name` directly when the session is not an un-handed-off overlay
/// run; otherwise buffer it for zero-loss handoff and open a new full Kitty
/// window on the first reveal trigger.
pub fn emit_or_buffer(app: &AppHandle, session_id: &str, event_name: &str, payload: Value) {
    let reveal: Option<(Value, RevealReason, Instant)> = {
        let state = app.state::<AppState>();
        let mut runs = state.overlay_runs.lock().unwrap();
        let Some(run) = runs.get_mut(session_id) else {
            drop(runs);
            let _ = app.emit(event_name, payload);
            return;
        };
        if run.handed_off {
            drop(runs);
            let _ = app.emit(event_name, payload);
            return;
        }
        let triggered = !run.window_opened
            && is_reveal_trigger(event_name, &payload, &mut run.in_leaked_think);
        let reason = if triggered {
            Some(match event_name {
                "chat://complete" => RevealReason::Complete,
                "chat://error" => {
                    let msg = payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("An error occurred.")
                        .to_string();
                    RevealReason::Error(msg)
                }
                _ => RevealReason::Output,
            })
        } else {
            None
        };
        run.buffered_events.push(json!({
            "event": event_name,
            "payload": payload,
        }));
        if let Some(r) = reason {
            run.window_opened = true;
            Some((build_handoff_payload(run), r, run.submitted_at))
        } else {
            None
        }
    };

    if let Some((handoff, reason, submitted_at)) = reveal {
        notifications::refresh_tray(app);
        open_revealed_window(app, session_id.to_string(), handoff, reason, submitted_at, false);
    }
}

/// If `session_id` belongs to an un-handed-off overlay run, reveal its new full
/// Kitty window (or keep it routed to its opening window) so the user can
/// answer an HITL prompt without hijacking an unrelated window.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn reveal_for_hitl(app: &AppHandle, pending: &crate::approvals::PendingApproval) -> bool {
    let action: Option<Option<(Value, Instant)>> = {
        let state = app.state::<AppState>();
        let mut runs = state.overlay_runs.lock().unwrap();
        match runs.get_mut(&pending.session_id) {
            None => None,
            Some(run) if run.handed_off => None,
            Some(run) if !run.window_opened => {
                run.window_opened = true;
                Some(Some((build_handoff_payload(run), run.submitted_at)))
            }
            Some(_) => Some(None),
        }
    };

    let Some(maybe_handoff) = action else {
        return false;
    };

    if let Some((handoff, submitted_at)) = maybe_handoff {
        notifications::refresh_tray(app);
        open_revealed_window(
            app,
            pending.session_id.clone(),
            handoff,
            RevealReason::Hitl {
                tool_name: pending.tool_name.clone(),
                scheduled: pending.scheduled,
            },
            submitted_at,
            false,
        );
    } else {
        // Window open is already in progress (mounting before `drain_overlay_run`):
        // focus it if already bound and post an approval toast if unfocused.
        #[cfg(not(target_os = "android"))]
        if let Some(label) = crate::windows::window_label_for_session(app, &pending.session_id) {
            crate::windows::show_and_focus(app, &label);
        }
        let title = if pending.scheduled {
            "A scheduled task needs your approval"
        } else {
            "Approval needed"
        };
        notifications::notify_if_hidden(
            app,
            notifications::Event::ApprovalNeeded,
            title,
            &format!("Kitty wants to run {}", pending.tool_name),
            Some(&pending.session_id),
        );
    }
    true
}

/// Build an `"overlay_run": true` handoff payload when `session_id` still has
/// an un-drained overlay run, used by `windows::focus_or_open_session` so
/// adopting the session drains `overlay_runs` instead of stranding live events.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn handoff_for_unhanded_run(app: &AppHandle, session_id: &str) -> Option<Value> {
    let payload = {
        let state = app.state::<AppState>();
        let mut runs = state.overlay_runs.lock().unwrap();
        let run = runs.get_mut(session_id)?;
        if run.handed_off {
            return None;
        }
        run.window_opened = true;
        build_handoff_payload(run)
    };
    notifications::refresh_tray(app);
    Some(payload)
}

/// True while `session_id` is an overlay run whose full window has not opened yet.
pub fn is_unrevealed(app: &AppHandle, session_id: &str) -> bool {
    let state = app.state::<AppState>();
    let runs = state.overlay_runs.lock().unwrap();
    runs.get(session_id)
        .is_some_and(|run| !run.window_opened)
}

/// Called when a turn finishes (`InFlight::drop`): prune expired dedup entries,
/// drop already-handed-off runs, or schedule a safety cleanup for un-drained
/// buffers so nothing leaks.
pub fn on_turn_finished(app: &AppHandle, session_id: &str) {
    let state = app.state::<AppState>();
    prune_recent_prompts(&state);

    let cleanup_ttl = {
        let mut runs = state.overlay_runs.lock().unwrap();
        match runs.get(session_id) {
            Some(run) if run.handed_off => {
                runs.remove(session_id);
                None
            }
            Some(run) => Some(if run.window_opened {
                OPENED_BUFFER_TTL
            } else {
                UNOPENED_BUFFER_TTL
            }),
            None => None,
        }
    };
    if let Some(ttl) = cleanup_ttl {
        let app_bg = app.clone();
        let sid = session_id.to_string();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(ttl).await;
            let state = app_bg.state::<AppState>();
            let busy = state
                .in_flight_sessions
                .lock()
                .unwrap()
                .contains(&sid);
            if !busy {
                state.overlay_runs.lock().unwrap().remove(&sid);
            }
        });
    }
}

/// Atomically mark `session_id` as handed off and drain its buffered events so
/// subsequent live events flow directly to the newly mounted full window.
///
/// Lock-order invariant: `handed_off = true` is committed under `overlay_runs`
/// *before* reading `in_flight_sessions`, matching `InFlight::drop` removing
/// from `in_flight_sessions` *before* `on_turn_finished` checks `handed_off`.
/// That guarantees a turn finishing concurrently is cleaned up by at least one
/// side without ever nesting the two mutexes.
pub fn drain_run(app: &AppHandle, session_id: &str) -> OverlayDrainResult {
    let state = app.state::<AppState>();
    let (events, had_entry) = {
        let mut runs = state.overlay_runs.lock().unwrap();
        match runs.get_mut(session_id) {
            Some(run) => {
                run.handed_off = true;
                (std::mem::take(&mut run.buffered_events), true)
            }
            None => (Vec::new(), false),
        }
    };
    let busy = state
        .in_flight_sessions
        .lock()
        .unwrap()
        .contains(session_id);
    if had_entry && !busy {
        state.overlay_runs.lock().unwrap().remove(session_id);
    }
    OverlayDrainResult { events, busy }
}

/// Unrevealed background overlay runs sorted chronologically: `(session_id, prompt_preview)`.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn unrevealed_tray_runs(app: &AppHandle) -> Vec<(String, String)> {
    let state = app.state::<AppState>();
    let runs = state.overlay_runs.lock().unwrap();
    let mut entries: Vec<(u64, String, String)> = runs
        .values()
        .filter(|run| !run.window_opened)
        .map(|run| (run.seq, run.session_id.clone(), run.prompt_preview.clone()))
        .collect();
    entries.sort_by_key(|(seq, _, _)| *seq);
    entries
        .into_iter()
        .map(|(_, sid, preview)| (sid, preview))
        .collect()
}

/// Immediately open and focus the chat window for an active background overlay run.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn reveal_now(app: &AppHandle, session_id: &str) {
    let pending: Option<(Value, Instant)> = {
        let state = app.state::<AppState>();
        let mut runs = state.overlay_runs.lock().unwrap();
        match runs.get_mut(session_id) {
            Some(run) if !run.handed_off && !run.window_opened => {
                run.window_opened = true;
                Some((build_handoff_payload(run), run.submitted_at))
            }
            _ => None,
        }
    };
    if let Some((handoff, submitted_at)) = pending {
        notifications::refresh_tray(app);
        open_revealed_window(
            app,
            session_id.to_string(),
            handoff,
            RevealReason::Output,
            submitted_at,
            true,
        );
        return;
    }
    let app_bg = app.clone();
    let sid = session_id.to_string();
    tauri::async_runtime::spawn(async move {
        crate::windows::focus_or_open_session(&app_bg, &sid).await;
    });
}

/// Cancel an in-flight background overlay run from the system tray without opening its window.
#[cfg_attr(not(desktop), allow(dead_code))]
pub fn cancel_background_run(app: &AppHandle, session_id: &str) {
    {
        let state = app.state::<AppState>();
        state.overlay_runs.lock().unwrap().remove(session_id);
    }
    notifications::refresh_tray(app);
    let app_bg = app.clone();
    let sid = session_id.to_string();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = crate::bigtiny::stream::cancel(&app_bg, &sid).await {
            tracing::warn!("tray cancel of background overlay run {sid} failed: {e}");
        }
    });
}

// ---------------------------------------------------------------------------
// Overlay submission entry point
// ---------------------------------------------------------------------------

fn surface_prestream_failure(app: &AppHandle, text: &str, err: &str) {
    let _ = app.emit(
        "overlay://submit-failed",
        json!({
            "text": text,
            "error": err,
        }),
    );
    let app_main = app.clone();
    let err_owned = err.to_string();
    if app
        .run_on_main_thread(move || {
            if crate::windows::show_overlay(&app_main).is_err() {
                notifications::notify_if_hidden(
                    &app_main,
                    notifications::Event::TaskFailed,
                    "Kitty couldn't start your chat",
                    &err_owned,
                    None,
                );
            }
        })
        .is_err()
    {
        notifications::notify_if_hidden(
            app,
            notifications::Event::TaskFailed,
            "Kitty couldn't start your chat",
            err,
            None,
        );
    }
}

/// Validate non-empty input, hide the overlay immediately, and run the prompt
/// in the background.
pub async fn submit_overlay_prompt(app: AppHandle, req: OverlayPromptRequest) -> Result<(), String> {
    let has_attachments = req
        .attached_paths
        .as_ref()
        .is_some_and(|p| !p.is_empty())
        || req.documents.as_ref().is_some_and(|d| !d.is_empty())
        || req.images.as_ref().is_some_and(|i| !i.is_empty());
    let has_slash_command = req.debate.unwrap_or(false)
        || req.research.is_some()
        || req
            .critique
            .as_deref()
            .is_some_and(|s| crate::bigtiny::critique::Stage::parse(s).is_some())
        || req.tidy.unwrap_or(false);
    if req.text.trim().is_empty() && !has_attachments && !has_slash_command {
        return Ok(());
    }

    let _ = crate::windows::hide_overlay(&app);

    let app_bg = app.clone();
    tauri::async_runtime::spawn(async move {
        let prompt_text = req.text.clone();
        if let Err(e) = run_overlay_submission(&app_bg, req).await {
            tracing::warn!("overlay submission failed before session stream started: {e}");
            surface_prestream_failure(&app_bg, &prompt_text, &e);
        }
    });
    Ok(())
}

async fn run_overlay_submission(app: &AppHandle, req: OverlayPromptRequest) -> Result<(), String> {
    let attachment_sig = build_attachment_signature(&req);
    let Some(record_id) = check_and_record_prompt(app, &req.text, attachment_sig).await else {
        tracing::info!("ignored rapid duplicate overlay prompt");
        return Ok(());
    };

    if let Err(e) = execute_recorded_submission(app, req, record_id).await {
        remove_recent_prompt(app, record_id);
        return Err(e);
    }
    Ok(())
}

async fn execute_recorded_submission(
    app: &AppHandle,
    req: OverlayPromptRequest,
    record_id: u64,
) -> Result<(), String> {
    if let Some(hist) = req
        .raw_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| (!req.text.trim().is_empty()).then_some(req.text.as_str()))
    {
        crate::config::record_overlay_prompt(app, hist);
    }
    let cwd_path = crate::commands::fresh_chat_folder(app).await?;
    let cwd = cwd_path.to_string_lossy().replace('\\', "/");

    let card = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        req.provider_id
            .as_deref()
            .and_then(|id| cfg.card(id))
            .filter(|p| p.is_usable())
            .or_else(|| {
                cfg.active_provider_id
                    .as_deref()
                    .and_then(|id| cfg.card(id))
                    .filter(|p| p.is_usable())
            })
    };
    let (provider, model) = match &card {
        Some(p) => (Some(p.id.clone()), p.models.first().cloned()),
        None => (None, None),
    };

    let mut info = crate::bigtiny::sessions::create(
        app,
        cwd.clone(),
        provider.clone(),
        model.clone(),
        provider.clone(),
    )
    .await?;
    let session_id = info.session_id.clone();
    attach_session_to_recent_prompt(app, record_id, &session_id);

    if let Some(custom_cwd) = req
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let c_owned = custom_cwd.to_string();
        let c_for_blocking = c_owned.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&c_for_blocking))
            .await
            .map_err(|e| format!("working-directory creation task panicked: {e}"))?
            .map_err(|e| format!("could not create the working directory {c_owned}: {e}"))?;
        let custom_norm = c_owned.replace('\\', "/");
        crate::bigtiny::sessions::update_cwd(app, &session_id, &custom_norm).await?;
        info.is_default_folder = crate::commands::is_default_folder(app, &custom_norm);
        info.cwd = custom_norm;
        if !info.is_default_folder {
            crate::config::record_recent_working_dir(app, &info.cwd);
        }
    }

    let persona = card
        .as_ref()
        .map(crate::config::providers::system_prompt_for)
        .unwrap_or_else(crate::config::providers::default_system_prompt);
    if let Err(e) =
        crate::bigtiny::sessions::update_persona_override(app, &session_id, &persona).await
    {
        tracing::warn!("could not set the new overlay chat's system prompt: {e}");
    }

    if let Some(effort_val) = req
        .thinking_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        {
            let state = app.state::<AppState>();
            let mut cfg = state.config.lock().unwrap();
            cfg.session_efforts
                .insert(session_id.clone(), effort_val.to_string());
            if let Err(e) = crate::config::save(&cfg) {
                tracing::warn!("could not persist overlay thinking effort: {e}");
            }
        }
        if let Err(e) =
            crate::bigtiny::sessions::update_thinking_effort(app, &session_id, effort_val).await
        {
            tracing::warn!("could not push overlay thinking effort: {e}");
        }
        info.thinking_effort =
            crate::bigtiny::effort::thinking_effort_for_session(app, &session_id).await;
    }

    // Confirm effort/context/vision once before sending, matching commands/session/prompt.rs.
    let first_turn = {
        let state = app.state::<AppState>();
        let mut confirmed = state.effort_confirmed_sessions.lock().unwrap();
        confirmed.insert(session_id.clone())
    };
    if first_turn {
        tokio::join!(
            crate::bigtiny::effort::confirm_model_effort(app, &session_id),
            crate::bigtiny::context_window::confirm_model_context_length(app, &session_id),
            crate::bigtiny::vision::confirm_model_vision(app, &session_id),
        );
    }

    // Stage attached files into cwd if outside tools_reachable_root, leaving directories intact.
    let mut raw_paths: Vec<String> = req
        .attached_paths
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect();
    if !raw_paths.is_empty() {
        let (dirs, files): (Vec<String>, Vec<String>) = raw_paths
            .into_iter()
            .partition(|p| std::path::Path::new(p).is_dir());
        let mut staged_files = if files.is_empty() {
            Vec::new()
        } else {
            match crate::commands::stage_attachments(files.clone(), cwd.clone()).await {
                Ok(staged) => staged.into_iter().map(|s| s.path).collect(),
                Err(e) => {
                    tracing::warn!("overlay stage_attachments failed: {e}");
                    files
                }
            }
        };
        staged_files.extend(dirs);
        raw_paths = staged_files;
    }

    let mut documents = req.documents.unwrap_or_default();
    let attached_paths_opt = (!raw_paths.is_empty()).then_some(raw_paths.clone());

    // Seed the user-message event BEFORE appending synthetic slash docs so the
    // full Kitty window shows only the user's own attachments cleanly.
    let mut initial_attachments: Vec<Value> = raw_paths
        .iter()
        .map(|p| {
            let name = std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.clone());
            json!({ "name": name, "kind": "file", "pinned": false })
        })
        .collect();
    for doc in &documents {
        initial_attachments.push(json!({
            "name": doc.label,
            "kind": "document",
            "pinned": doc.pinned,
        }));
    }
    if let Some(imgs) = &req.images {
        for i in 0..imgs.len() {
            let name = if imgs.len() > 1 {
                format!("Image {}", i + 1)
            } else {
                "Image".to_string()
            };
            initial_attachments.push(json!({
                "name": name,
                "kind": "image",
                "pinned": false,
            }));
        }
    }

    let debate_panel = if req.debate.unwrap_or(false) {
        let turn = crate::bigtiny::debate::prepare(
            app,
            &session_id,
            &documents,
            &raw_paths,
        )
        .await;
        documents.push(turn.brief);
        turn.panel
    } else {
        None
    };
    let debate_panel = match req
        .critique
        .as_deref()
        .and_then(crate::bigtiny::critique::Stage::parse)
    {
        Some(stage) => {
            let turn = crate::bigtiny::critique::prepare(
                app,
                &session_id,
                stage,
                &req.text,
                &documents,
                &raw_paths,
            )
            .await;
            documents.extend(turn.playbook);
            turn.panel
        }
        None => debate_panel,
    };
    if let Some(args) = req.research {
        let client = crate::bigtiny::client::ensure_client(app)?;
        let started = crate::bigtiny::research::start(&client, &session_id, &req.text, &args)
            .await
            .map_err(|e| {
                tracing::warn!("deep research start for {session_id}: {e}");
                if e.contains("already has a research run") {
                    "This chat already has a deep research run in progress.".to_string()
                } else {
                    "Could not start deep research.".to_string()
                }
            })?;
        documents.push(crate::bigtiny::research::instructions_document(&started));
    }
    if req.tidy.unwrap_or(false) {
        let doc = crate::bigtiny::tidy::prepare(app, &session_id, &req.text)
            .await
            .map_err(|e| {
                tracing::warn!("tidy prepare for {session_id}: {e}");
                if e.contains("answered 404") {
                    "Kitty's engine is older than this Kitty and has no /tidy yet. It updates \
                     when it restarts: on its own once nothing is using it, or from the banner."
                        .to_string()
                } else {
                    "Could not read the chats to tidy.".to_string()
                }
            })?;
        documents.push(doc);
    }

    let composed_text = crate::bigtiny::turn_text::compose(&req.text, &documents, &raw_paths);
    let has_any_att = !initial_attachments.is_empty();
    let display_text = if req.text.trim().is_empty() {
        req.raw_prompt.as_deref().unwrap_or("").trim()
    } else {
        req.text.trim()
    };
    let preview_source = req
        .raw_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&req.text);
    let preview = make_prompt_preview(preview_source, has_any_att);
    let user_msg_event = json!({
        "event": "chat://user-message",
        "payload": {
            "session_id": session_id,
            "text": display_text,
            "attachments": initial_attachments,
        }
    });

    {
        let state = app.state::<AppState>();
        state.overlay_runs.lock().unwrap().insert(
            session_id.clone(),
            OverlayRunState {
                seq: RUN_SEQ.fetch_add(1, Ordering::Relaxed),
                submitted_at: Instant::now(),
                session_id: session_id.clone(),
                cwd: info.cwd.clone(),
                prompt_preview: preview,
                provider_id: info.provider_id.clone(),
                model_id: info.model_id.clone(),
                thinking_effort: info.thinking_effort.clone(),
                is_default_folder: info.is_default_folder,
                window_opened: false,
                handed_off: false,
                buffered_events: vec![user_msg_event],
                in_leaked_think: false,
            },
        );
    }
    notifications::refresh_tray(app);

    if let Err(err) = crate::bigtiny::stream::send_prompt(
        app.clone(),
        session_id.clone(),
        composed_text,
        req.images,
        attached_paths_opt,
        debate_panel,
    )
    .await
    {
        // Allow immediate retry of a prompt whose send stream failed to open.
        remove_recent_prompt(app, record_id);
        emit_or_buffer(
            app,
            &session_id,
            "chat://error",
            json!({
                "session_id": session_id,
                "message": err,
                "error_type": null,
            }),
        );
        on_turn_finished(app, &session_id);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_matches_semantically_equivalent_flower_shop_queries() {
        let a = "find me information about flower shops in los angeles";
        let b = "find me information about florists in LA";
        assert!(lexical_fallback_match(a, b));
    }

    #[test]
    fn dedup_rejects_different_subject_and_city_with_same_template() {
        let a = "find me information about florists in LA";
        let b = "find me information about barbershops in cleveland";
        assert!(!lexical_fallback_match(a, b));
    }

    #[test]
    fn dedup_handles_other_domains_and_respects_word_boundaries() {
        assert!(lexical_fallback_match(
            "Could you please look up Python async tutorials?",
            "please search for python async tutorial"
        ));
        assert!(!lexical_fallback_match(
            "debug rust borrow checker error",
            "debug postgres connection pool timeout"
        ));
        // Word-boundary check: "la" alias must not corrupt words containing "la"
        assert!(!lexical_fallback_match("best laptops", "best los angeles"));
    }

    #[test]
    fn dedup_strips_shared_prefix_without_inflating_unrelated_queries() {
        let (ca, cb) = strip_shared_prefix_and_filler(
            "find me information about florists in LA",
            "find me information about barbershops in cleveland",
        );
        assert_eq!(ca, "florists in la");
        assert_eq!(cb, "barbershops in cleveland");
    }

    #[test]
    fn cosine_and_embedding_thresholds_work_as_expected() {
        let v1 = [1.0f32, 0.0, 0.0];
        let v2 = [0.9f32, 0.4358899, 0.0];
        let v3 = [0.0f32, 1.0, 0.0];
        assert!((cosine_similarity(&v1, &v1) - 1.0).abs() < 1e-5);
        assert!(cosine_similarity(&v1, &v2) > 0.89);
        assert!(cosine_similarity(&v1, &v3).abs() < 1e-5);

        assert!(embeddings_match(0.89, Some(0.50)));
        assert!(embeddings_match(0.80, Some(0.75)));
        assert!(!embeddings_match(0.82, Some(0.45)));
        assert!(!embeddings_match(0.70, Some(0.95)));
    }

    #[test]
    fn reveal_trigger_waits_for_visible_output_outside_think_tags() {
        let mut in_think = false;
        assert!(!is_reveal_trigger(
            "chat://reasoning-delta",
            &json!({"text": "pondering..."}),
            &mut in_think
        ));
        assert!(!is_reveal_trigger(
            "chat://tool-call",
            &json!({"phase": "tool_call"}),
            &mut in_think
        ));
        assert!(!is_reveal_trigger(
            "chat://message-delta",
            &json!({"text": "   \n  "}),
            &mut in_think
        ));
        assert!(!is_reveal_trigger(
            "chat://message-delta",
            &json!({"text": "<think>hidden chain"}),
            &mut in_think
        ));
        assert!(in_think);
        assert!(!is_reveal_trigger(
            "chat://message-delta",
            &json!({"text": " still thinking</think>   "}),
            &mut in_think
        ));
        assert!(!in_think);
        assert!(is_reveal_trigger(
            "chat://message-delta",
            &json!({"text": "Here is the answer."}),
            &mut in_think
        ));
        assert!(is_reveal_trigger(
            "chat://error",
            &json!({"message": "boom"}),
            &mut in_think
        ));
        assert!(is_reveal_trigger(
            "chat://complete",
            &json!({"result": {}}),
            &mut in_think
        ));
    }
}
