//! OpenRouter's public model catalog, cached and reused as the universal
//! cost/capability/age ranking source for the provider-add model picker —
//! not just for OpenRouter itself. OpenRouter's `GET /api/v1/models`
//! response carries real per-token pricing and, per-model, Artificial
//! Analysis's `intelligence_index`/`coding_index`/`agentic_index`
//! (confirmed live: present under `benchmarks.artificial_analysis` on every
//! response, sorted or not — no special query param needed to get them).
//! Every other provider type validates its own key directly against its own
//! `/v1/models`-equivalent (see `commands/provider.rs::discover_provider_models`)
//! and only consults this cache to *cross-reference* cost/capability/age for
//! whatever models that vendor actually returns.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::util::http_client;

/// A model is considered stale enough to refetch after this long — checked
/// lazily, at the point of use (opening Add/Edit Provider), not on a
/// background timer. A session can stay open for days; a timer would need
/// its own always-on scheduler for data nothing else needs kept warm.
const STALE_AFTER_SECS: i64 = 6 * 60 * 60;

/// Simplified cost badge — bucketed by percentile against the *current*
/// catalog (terciles), not a fixed dollar table, so it tracks market pricing
/// drift automatically. Mirrors TS `CostTier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostTier {
    Economy,
    Moderate,
    Premium,
}

/// One model from OpenRouter's catalog, with everything the picker needs
/// already extracted. Fields are `Option` throughout — OpenRouter doesn't
/// price or benchmark every model (audio/embedding/free entries especially),
/// and a missing field must never become a fabricated value downstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterCatalogEntry {
    /// OpenRouter's own slug, e.g. `"anthropic/claude-sonnet-5"`.
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub created: Option<i64>,
    pub context_length: Option<u32>,
    /// $/token (OpenRouter returns these as stringified decimals).
    pub pricing_prompt: Option<f64>,
    pub pricing_completion: Option<f64>,
    pub intelligence_index: Option<f64>,
    pub coding_index: Option<f64>,
    pub agentic_index: Option<f64>,
    /// Blended $/M-tokens (prompt weighted 0.75, completion 0.25 — prompt is
    /// typically the larger share of a turn), used both for the "Cheapest"
    /// sort and to derive `cost_tier`.
    pub price_rank: Option<f64>,
    pub cost_tier: Option<CostTier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterCatalog {
    pub fetched_at: i64,
    pub entries: Vec<OpenRouterCatalogEntry>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `GET /api/v1/models` — fetches the full catalog and extracts everything
/// the picker needs. `bearer`: pass the user's just-entered key when this is
/// called as a cold-cache fallback from inside a validation request (moot —
/// the list itself is public — but harmless, and saves a branch).
pub async fn fetch_catalog(bearer: Option<&str>) -> Result<OpenRouterCatalog, String> {
    let mut req = http_client()
        .get("https://openrouter.ai/api/v1/models")
        .timeout(std::time::Duration::from_secs(15));
    if let Some(key) = bearer {
        req = req.bearer_auth(key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("could not reach OpenRouter: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| e.to_string())?;
    let raw = json
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();

    let mut entries: Vec<OpenRouterCatalogEntry> =
        raw.iter().filter_map(parse_entry).collect();
    assign_cost_tiers(&mut entries);

    Ok(OpenRouterCatalog {
        fetched_at: now_unix(),
        entries,
    })
}

fn parse_entry(v: &Value) -> Option<OpenRouterCatalogEntry> {
    let id = v.get("id")?.as_str()?.to_string();
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or(&id)
        .to_string();
    let created = v.get("created").and_then(|c| c.as_i64());
    let context_length = v
        .get("context_length")
        .and_then(|c| c.as_u64())
        .map(|n| n as u32);

    let pricing = v.get("pricing");
    let pricing_prompt = pricing
        .and_then(|p| p.get("prompt"))
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let pricing_completion = pricing
        .and_then(|p| p.get("completion"))
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<f64>().ok());

    let aa = v.get("benchmarks").and_then(|b| b.get("artificial_analysis"));
    let intelligence_index = aa
        .and_then(|a| a.get("intelligence_index"))
        .and_then(|x| x.as_f64());
    let coding_index = aa
        .and_then(|a| a.get("coding_index"))
        .and_then(|x| x.as_f64());
    let agentic_index = aa
        .and_then(|a| a.get("agentic_index"))
        .and_then(|x| x.as_f64());

    let price_rank = blended_price_rank(pricing_prompt, pricing_completion);

    Some(OpenRouterCatalogEntry {
        id,
        name,
        created,
        context_length,
        pricing_prompt,
        pricing_completion,
        intelligence_index,
        coding_index,
        agentic_index,
        price_rank,
        cost_tier: None, // filled in by assign_cost_tiers once the whole catalog is in hand
    })
}

/// $/token -> a blended $/M-tokens figure. Missing one side (e.g. an
/// embedding model with no completion price) still ranks on whichever side
/// exists rather than being excluded outright.
fn blended_price_rank(prompt: Option<f64>, completion: Option<f64>) -> Option<f64> {
    match (prompt, completion) {
        (Some(p), Some(c)) => Some(p * 1_000_000.0 * 0.75 + c * 1_000_000.0 * 0.25),
        (Some(p), None) => Some(p * 1_000_000.0),
        (None, Some(c)) => Some(c * 1_000_000.0),
        (None, None) => None,
    }
}

/// Tercile-bucket every entry that has a `price_rank`, cheapest third ->
/// `Economy`, middle -> `Moderate`, priciest -> `Premium`. Entries with no
/// pricing data are left at `cost_tier: None` — never a fabricated bucket.
fn assign_cost_tiers(entries: &mut [OpenRouterCatalogEntry]) {
    let mut priced: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.price_rank.is_some())
        .map(|(i, _)| i)
        .collect();
    if priced.is_empty() {
        return;
    }
    priced.sort_by(|&a, &b| {
        entries[a]
            .price_rank
            .partial_cmp(&entries[b].price_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let n = priced.len();
    for (rank, &i) in priced.iter().enumerate() {
        let tier = if rank < n / 3 {
            CostTier::Economy
        } else if rank < (n * 2) / 3 {
            CostTier::Moderate
        } else {
            CostTier::Premium
        };
        entries[i].cost_tier = Some(tier);
    }
}

fn cache_path() -> Result<PathBuf, String> {
    let dir = crate::config::config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("openrouter_catalog.json"))
}

/// Best-effort disk read for a warm cold-start — `None` on any failure
/// (missing file, corrupt JSON, unreadable path), never an error the caller
/// needs to handle specially.
pub fn load_disk_cache() -> Option<OpenRouterCatalog> {
    let path = cache_path().ok()?;
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_disk_cache(catalog: &OpenRouterCatalog) -> Result<(), String> {
    let path = cache_path()?;
    let data = serde_json::to_string(catalog).map_err(|e| e.to_string())?;
    std::fs::write(path, data).map_err(|e| e.to_string())
}

/// Normalize a vendor-specific model id down to a comparable form:
/// - keep only the segment after the last `/` (Fireworks'
///   `accounts/fireworks/models/qwen3-235b-a22b-instruct`, DeepInfra's
///   `Qwen/Qwen3-235B-A22B` both collapse to their bare model name; a no-op
///   for QwenCloud's already-flat `qwen-max`)
/// - lowercase
/// - strip a trailing `-YYYYMMDD`/`-YYYYMM`-shaped date suffix (Anthropic's
///   dated snapshot ids, e.g. `claude-sonnet-5-20260101`)
/// - collapse `_`/`.`/repeated `-` into single `-` separators
pub fn normalize_model_id(raw: &str) -> String {
    let last = raw.rsplit('/').next().unwrap_or(raw);
    let lower = last.to_lowercase();
    let stripped = strip_trailing_date_suffix(&lower);
    collapse_separators(stripped)
}

fn strip_trailing_date_suffix(s: &str) -> &str {
    if let Some(pos) = s.rfind('-') {
        let tail = &s[pos + 1..];
        if (6..=8).contains(&tail.len()) && tail.chars().all(|c| c.is_ascii_digit()) {
            return &s[..pos];
        }
    }
    s
}

fn collapse_separators(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_sep = false;
    for c in s.chars() {
        if c == '_' || c == '.' || c == '-' {
            if !last_was_sep && !out.is_empty() {
                out.push('-');
            }
            last_was_sep = true;
        } else {
            out.push(c);
            last_was_sep = false;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Look up a raw (un-normalized) vendor model id in the catalog. Tries an
/// exact normalized-id match first, then an exact normalized-*name* match,
/// then a length-ratio-gated substring match (catches near-miss id
/// spellings without letting a short string like `"qwen"` spuriously match
/// every Qwen variant). `None` on no match — logged by the caller at
/// `debug!`, never surfaced to the user as an error.
pub fn match_in_catalog<'a>(
    raw_id: &str,
    entries: &'a [OpenRouterCatalogEntry],
) -> Option<&'a OpenRouterCatalogEntry> {
    let needle = normalize_model_id(raw_id);
    if needle.is_empty() {
        return None;
    }

    if let Some(e) = entries.iter().find(|e| normalize_model_id(&e.id) == needle) {
        return Some(e);
    }
    if let Some(e) = entries
        .iter()
        .find(|e| normalize_model_id(&e.name) == needle)
    {
        return Some(e);
    }
    entries.iter().find(|e| {
        let hay = normalize_model_id(&e.id);
        substring_match(&needle, &hay)
    })
}

/// One string contains the other, and the shorter isn't so much shorter
/// that the match is likely coincidental (e.g. bare `"qwen"` inside
/// `"qwen3-235b-a22b-instruct"` — ratio ~0.17, rejected).
fn substring_match(a: &str, b: &str) -> bool {
    let (shorter, longer) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if shorter.is_empty() || !longer.contains(shorter) {
        return false;
    }
    // 0.6, not the stricter 0.7 first tried: a real vendor variant suffix
    // like "-instruct" trims the ratio to ~0.67 for a genuine match
    // ("qwen3-235b-a22b" vs "qwen3-235b-a22b-instruct") — 0.6 still comfortably
    // rejects a bare "qwen" (ratio ~0.17) inside the same longer id.
    (shorter.len() as f64 / longer.len() as f64) >= 0.6
}

/// One shared Mutex-guarded slot the whole app reads/writes through — see
/// `AppState::openrouter_catalog`. Kept here (not in `state.rs`) so all the
/// catalog logic — fetch, cache, freshness — stays in one module.
pub type CatalogSlot = Mutex<Option<OpenRouterCatalog>>;

/// Refresh `state`'s cached catalog if it's empty or more than
/// [`STALE_AFTER_SECS`] old. Called from `discover_provider_models`/
/// `discover_provider_models_for_saved` — opening the Add/Edit Provider form
/// is what keeps this from going stale over a long-running session, not a
/// background timer. A failed refresh leaves whatever was already cached in
/// place (even if stale) rather than clearing it — callers proceed with
/// best-effort data either way, never blocked on this succeeding.
pub async fn ensure_catalog_fresh(state: &crate::state::AppState) {
    let needs_refresh = {
        let guard = state.openrouter_catalog.lock().unwrap();
        match guard.as_ref() {
            Some(c) => now_unix() - c.fetched_at > STALE_AFTER_SECS,
            None => true,
        }
    };
    if !needs_refresh {
        return;
    }
    match fetch_catalog(None).await {
        Ok(fresh) => {
            let _ = save_disk_cache(&fresh);
            *state.openrouter_catalog.lock().unwrap() = Some(fresh);
        }
        Err(e) => {
            tracing::warn!("OpenRouter catalog refresh failed, keeping stale cache: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, price_rank: Option<f64>) -> OpenRouterCatalogEntry {
        OpenRouterCatalogEntry {
            id: id.to_string(),
            name: id.to_string(),
            created: None,
            context_length: None,
            pricing_prompt: None,
            pricing_completion: None,
            intelligence_index: None,
            coding_index: None,
            agentic_index: None,
            price_rank,
            cost_tier: None,
        }
    }

    #[test]
    fn normalize_strips_vendor_prefix_and_lowercases() {
        assert_eq!(normalize_model_id("anthropic/claude-sonnet-5"), "claude-sonnet-5");
    }

    #[test]
    fn normalize_strips_fireworks_deep_path() {
        assert_eq!(
            normalize_model_id("accounts/fireworks/models/qwen3-235b-a22b-instruct"),
            "qwen3-235b-a22b-instruct"
        );
    }

    #[test]
    fn normalize_strips_deepinfra_mixed_case_prefix() {
        assert_eq!(normalize_model_id("Qwen/Qwen3-235B-A22B"), "qwen3-235b-a22b");
    }

    #[test]
    fn normalize_strips_anthropic_dated_snapshot_suffix() {
        assert_eq!(
            normalize_model_id("claude-sonnet-5-20260101"),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn normalize_leaves_flat_qwen_cloud_id_unchanged() {
        assert_eq!(normalize_model_id("qwen-max"), "qwen-max");
    }

    #[test]
    fn normalize_does_not_strip_a_short_numeric_suffix() {
        // "27b" is not a 6-8 digit date — must survive intact.
        assert_eq!(normalize_model_id("qwen3.8-27b"), "qwen3-8-27b");
    }

    #[test]
    fn normalize_collapses_underscores_and_dots() {
        assert_eq!(normalize_model_id("gpt_4.1__mini"), "gpt-4-1-mini");
    }

    #[test]
    fn match_in_catalog_exact_id_hit() {
        let entries = vec![entry("anthropic/claude-sonnet-5", Some(1.0))];
        let m = match_in_catalog("claude-sonnet-5-20260101", &entries);
        assert_eq!(m.unwrap().id, "anthropic/claude-sonnet-5");
    }

    #[test]
    fn match_in_catalog_no_hit_returns_none() {
        let entries = vec![entry("anthropic/claude-sonnet-5", Some(1.0))];
        assert!(match_in_catalog("some-totally-unrelated-model", &entries).is_none());
    }

    #[test]
    fn substring_match_rejects_short_false_positive() {
        assert!(!substring_match("qwen", "qwen3-235b-a22b-instruct"));
    }

    #[test]
    fn substring_match_accepts_close_variant() {
        assert!(substring_match("qwen3-235b-a22b", "qwen3-235b-a22b-instruct"));
    }

    #[test]
    fn cost_tier_bucketing_splits_into_terciles() {
        let mut entries: Vec<OpenRouterCatalogEntry> = (0..9)
            .map(|i| entry(&format!("m{i}"), Some(i as f64)))
            .collect();
        assign_cost_tiers(&mut entries);
        let tiers: Vec<_> = entries.iter().map(|e| e.cost_tier).collect();
        assert_eq!(
            tiers,
            vec![
                Some(CostTier::Economy),
                Some(CostTier::Economy),
                Some(CostTier::Economy),
                Some(CostTier::Moderate),
                Some(CostTier::Moderate),
                Some(CostTier::Moderate),
                Some(CostTier::Premium),
                Some(CostTier::Premium),
                Some(CostTier::Premium),
            ]
        );
    }

    #[test]
    fn cost_tier_bucketing_leaves_unpriced_entries_untiered() {
        let mut entries = vec![entry("priced", Some(5.0)), entry("unpriced", None)];
        assign_cost_tiers(&mut entries);
        assert!(entries[0].cost_tier.is_some());
        assert!(entries[1].cost_tier.is_none());
    }

    #[test]
    fn cost_tier_bucketing_empty_catalog_does_not_panic() {
        let mut entries: Vec<OpenRouterCatalogEntry> = vec![];
        assign_cost_tiers(&mut entries);
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_entry_reads_real_openrouter_shape() {
        let v = serde_json::json!({
            "id": "anthropic/claude-opus-5",
            "name": "Claude Opus 5",
            "created": 1784912544,
            "context_length": 1000000,
            "pricing": {
                "prompt": "0.000005",
                "completion": "0.000025"
            },
            "benchmarks": {
                "artificial_analysis": {
                    "intelligence_index": 63.1,
                    "coding_index": 78,
                    "agentic_index": 59.2
                }
            }
        });
        let e = parse_entry(&v).unwrap();
        assert_eq!(e.id, "anthropic/claude-opus-5");
        assert_eq!(e.created, Some(1784912544));
        assert_eq!(e.context_length, Some(1_000_000));
        assert_eq!(e.pricing_prompt, Some(0.000005));
        assert_eq!(e.pricing_completion, Some(0.000025));
        assert_eq!(e.intelligence_index, Some(63.1));
        assert_eq!(e.coding_index, Some(78.0));
        assert_eq!(e.agentic_index, Some(59.2));
        assert!(e.price_rank.is_some());
    }

    #[test]
    fn parse_entry_missing_pricing_and_benchmarks_is_still_ok() {
        let v = serde_json::json!({
            "id": "some/model",
            "name": "Some Model"
        });
        let e = parse_entry(&v).unwrap();
        assert_eq!(e.pricing_prompt, None);
        assert_eq!(e.intelligence_index, None);
        assert_eq!(e.price_rank, None);
    }

    #[test]
    fn parse_entry_requires_an_id() {
        let v = serde_json::json!({ "name": "no id here" });
        assert!(parse_entry(&v).is_none());
    }
}
