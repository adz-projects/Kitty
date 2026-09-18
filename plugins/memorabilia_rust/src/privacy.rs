//! Privacy (plan §15 Phase 3, §12): the Stage 0 PII gate, the
//! `delete`/`forget` ladder, and the tombstone granularity guard.
//!
//! Invariants pinned by tests:
//! * PII is rejected **before any write to disk**; audit rows carry the
//!   PII **category only**, never the value (claude.md principle 6).
//! * `private` = hard delete with cascade (chunk → proposition → edge →
//!   vector, one transaction) + permanent tombstone, so the text is never
//!   relearned.
//! * `wrong` = permanent suppression + tombstone; `outdated` =
//!   time-bounded suppression, no tombstone.
//! * Tombstones are recorded only when the deleted text clears
//!   `tombstone_min_chars` or carries a high-entropy token, so short
//!   generic phrases are deleted without poisoning universal patterns.
//!
//! The detection patterns are deterministic and hand-rolled (no regex
//! crate): the gate must run hot and dependency-light, and the entity
//! classes (email, phone, SSN, card, API key, address) are each a small
//! structural scan.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

use crate::config::Config;
use crate::engine::Engine;
use crate::error::Error;
use crate::store::audit::AuditEntry;
use crate::store::tombstones::{Suppression, Tombstone};
use crate::text::normalize_text;

/// ISO-8601 UTC second precision — the format every timestamp column holds
/// (migration 002 convention).
const TIMESTAMP_FMT: &str = "%Y-%m-%dT%H:%M:%SZ";

// ---------------------------------------------------------------------------
// Stage 0 — PII gate (plan §12.1)
// ---------------------------------------------------------------------------

/// Deterministic PII categories (plan §12.1: email, phone, SSN,
/// credit card, API key/secret, address).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PiiCategory {
    Email,
    Phone,
    Ssn,
    Card,
    ApiKey,
    Address,
}

impl PiiCategory {
    /// The category string audit rows record — **category only, never the
    /// value** (claude.md principle 6).
    pub fn as_str(self) -> &'static str {
        match self {
            PiiCategory::Email => "email",
            PiiCategory::Phone => "phone",
            PiiCategory::Ssn => "ssn",
            PiiCategory::Card => "card",
            PiiCategory::ApiKey => "api_key",
            PiiCategory::Address => "address",
        }
    }
}

/// Optional LLM classifier seam (plan §12.1, flagged `#privacy`): a
/// conservative check for unstructured PII the deterministic patterns miss
/// (names + personal context). Production may implement it over a local
/// model; tests use [`MockPiiClassifier`]; a host that wants regex-only
/// detection simply does not install one.
#[async_trait]
pub trait PiiClassifier: Send + Sync {
    /// Classify `text`. Returns the PII category to record (taxonomy
    /// strings like [`PiiCategory::as_str`] recommended), or `None` when
    /// the classifier finds nothing. Errors are soft-failed by the gate:
    /// the deterministic result stands, as if no classifier were installed.
    async fn classify(&self, text: &str) -> Result<Option<String>, String>;
}

/// Test double for [`PiiClassifier`]: returns a canned category, or a
/// canned error when `error` is set (soft-fail path).
#[derive(Debug, Clone, Default)]
pub struct MockPiiClassifier {
    pub category: Option<String>,
    pub error: Option<String>,
}

#[async_trait]
impl PiiClassifier for MockPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<Option<String>, String> {
        match &self.error {
            Some(e) => Err(e.clone()),
            None => Ok(self.category.clone()),
        }
    }
}

/// The Stage 0 gate (plan §12.1): deterministic detection always runs; the
/// optional LLM classifier runs second and may add one category. Returns
/// the **categories only**, deduplicated, in scan order — no PII value
/// ever leaves this function. An empty result means "clear to ingest".
pub async fn pii_categories(
    classifier: Option<&Arc<dyn PiiClassifier>>,
    text: &str,
) -> Vec<String> {
    let mut out: Vec<String> = scan_pii(text)
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();
    if let Some(clf) = classifier {
        match clf.classify(text).await {
            Ok(Some(cat)) => {
                let cat = cat.trim().to_lowercase();
                if !cat.is_empty() && !out.iter().any(|c| *c == cat) {
                    out.push(cat);
                }
            }
            Ok(None) => {}
            // Soft-fail: a broken classifier never blocks ingestion beyond
            // what the deterministic patterns already decide.
            Err(e) => {
                tracing::warn!("pii gate: LLM classifier failed ({e}); deterministic result stands")
            }
        }
    }
    out
}

/// Deterministic detector: one structural scan per entity class.
/// Conservative by design — the gate would rather reject a borderline
/// chunk than leak PII — but each scan is shaped to keep false positives
/// low on timestamps, version strings, and ordinary prose.
pub fn scan_pii(text: &str) -> Vec<PiiCategory> {
    let mut cats = Vec::new();
    if find_email(text) {
        cats.push(PiiCategory::Email);
    }
    if find_ssn(text) {
        cats.push(PiiCategory::Ssn);
    }
    if find_credit_card(text) {
        cats.push(PiiCategory::Card);
    }
    if find_api_key(text) {
        cats.push(PiiCategory::ApiKey);
    }
    if find_phone(text) {
        cats.push(PiiCategory::Phone);
    }
    if find_address(text) {
        cats.push(PiiCategory::Address);
    }
    cats
}

fn find_email(text: &str) -> bool {
    let cs: Vec<char> = text.to_lowercase().chars().collect();
    let n = cs.len();
    for (i, &c) in cs.iter().enumerate() {
        if c != '@' {
            continue;
        }
        let is_local = |c: char| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
        };
        let mut j = i;
        while j > 0 && is_local(cs[j - 1]) {
            j -= 1;
        }
        let local = &cs[j..i];
        if local.is_empty() || local.len() > 64 {
            continue;
        }
        if local[0] == '.' || local[local.len() - 1] == '.' {
            continue;
        }
        let is_domain = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-';
        let mut k = i + 1;
        while k < n && is_domain(cs[k]) {
            k += 1;
        }
        let domain: String = cs[i + 1..k].iter().collect();
        if domain.len() < 3 || domain.len() > 253 {
            continue;
        }
        if domain.starts_with('.')
            || domain.ends_with('.')
            || domain.starts_with('-')
            || domain.ends_with('-')
        {
            continue;
        }
        // a dot with an alphabetic TLD of >= 2 labels
        if let Some(dot) = domain.rfind('.') {
            let tld = &domain[dot + 1..];
            if tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()) {
                return true;
            }
        }
    }
    false
}

fn find_ssn(text: &str) -> bool {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    // exactly \d{3}-\d{2}-\d{4}, not embedded in a longer digit run
    for i in 0..=n.saturating_sub(11) {
        if cs[i].is_ascii_digit()
            && cs[i + 1].is_ascii_digit()
            && cs[i + 2].is_ascii_digit()
            && cs[i + 3] == '-'
            && cs[i + 4].is_ascii_digit()
            && cs[i + 5].is_ascii_digit()
            && cs[i + 6] == '-'
            && cs[i + 7].is_ascii_digit()
            && cs[i + 8].is_ascii_digit()
            && cs[i + 9].is_ascii_digit()
            && cs[i + 10].is_ascii_digit()
            && (i == 0 || !cs[i - 1].is_ascii_digit())
            && (i + 11 == n || !cs[i + 11].is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Luhn check over a digit string (credit-card entity pattern, plan
/// §12.1). 13–19 significant digits with a valid Luhn sum: a random digit
/// run of that length passes with probability ~1/10, and real prose almost
/// never contains one.
fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut dbl = false;
    for c in digits.chars().rev() {
        let d = c.to_digit(10).unwrap_or(0);
        let d = if dbl { d * 2 } else { d };
        sum += if d > 9 { d - 9 } else { d };
        dbl = !dbl;
    }
    sum % 10 == 0
}

fn find_credit_card(text: &str) -> bool {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    let mut i = 0;
    while i < n {
        if cs[i].is_ascii_digit() {
            let mut j = i;
            let mut digits = String::new();
            while j < n {
                if cs[j].is_ascii_digit() {
                    digits.push(cs[j]);
                    j += 1;
                } else if (cs[j] == ' ' || cs[j] == '-') && j + 1 < n && cs[j + 1].is_ascii_digit()
                {
                    // internal separator; a trailing one terminates the run
                    j += 1;
                } else {
                    break;
                }
            }
            if digits.len() >= 13 && digits.len() <= 19 && luhn_valid(&digits) {
                return true;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// API key / secret patterns (plan §12.1): well-known key prefixes,
/// `name = value` credential assignments, and long mixed-case alphanumeric
/// runs (the shape of generated secrets).
fn find_api_key(text: &str) -> bool {
    let lower = text.to_lowercase();
    let cs: Vec<char> = lower.chars().collect();
    // 1) attached prefixes: the key follows the marker directly
    const PREFIXES: &[&str] = &[
        "sk-", "akia", "ghp_", "gho_", "ghs_", "ghu_", "xoxb-", "xoxp-", "aiza",
    ];
    for p in PREFIXES {
        if let Some(start) = lower.find(p) {
            let mut count = 0usize;
            for &c in &cs[start + p.len()..] {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    count += 1;
                } else {
                    break;
                }
            }
            if count >= 16 {
                return true;
            }
        }
    }
    // 2) named assignments: api_key / apikey / secret_key / access_token
    //    (also with a space), then an optional `=`/`:` and the value.
    const NAMES: &[&str] = &["api_key", "apikey", "api key", "secret_key", "secret key", "access_token"];
    for name in NAMES {
        if let Some(start) = lower.find(name) {
            let mut i = start + name.len();
            // up to 8 spaces, then an optional separator, up to 8 more spaces
            let mut seen_sep = false;
            while i < cs.len() && cs[i] == ' ' {
                i += 1;
            }
            if i < cs.len() && (cs[i] == '=' || cs[i] == ':') {
                seen_sep = true;
                i += 1;
            }
            while i < cs.len() && cs[i] == ' ' {
                i += 1;
            }
            let _ = seen_sep;
            if i < cs.len() && matches!(cs[i], '\'' | '"' | '`') {
                i += 1;
            }
            let mut count = 0usize;
            while i < cs.len() && is_secret_char(cs[i]) {
                count += 1;
                i += 1;
            }
            if count >= 16 {
                return true;
            }
        }
    }
    // 3) bare high-entropy run: >= 32 chars mixing upper+lower+digits
    //    (pure hex digests deliberately excluded — documents quote them).
    for run in bare_runs(&cs) {
        if run.len() >= 32
            && run.chars().any(|c| c.is_ascii_uppercase())
            && run.chars().any(|c| c.is_ascii_lowercase())
            && run.chars().any(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')
}

fn bare_runs(cs: &[char]) -> Vec<String> {
    let mut runs = Vec::new();
    let mut cur = String::new();
    for &c in cs {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            if cur.len() >= 16 {
                runs.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() >= 16 {
        runs.push(cur);
    }
    runs
}

fn find_phone(text: &str) -> bool {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    let is_phone_char = |c: char| {
        c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | '(' | ')' | ' ')
    };
    let mut i = 0;
    while i < n {
        if is_phone_char(cs[i]) {
            let mut j = i;
            let mut groups: Vec<String> = Vec::new();
            let mut cur = String::new();
            let mut has_sep = false;
            while j < n && is_phone_char(cs[j]) {
                if cs[j].is_ascii_digit() {
                    cur.push(cs[j]);
                } else if !cur.is_empty() {
                    has_sep = true;
                    groups.push(std::mem::take(&mut cur));
                }
                j += 1;
            }
            if !cur.is_empty() {
                groups.push(cur);
            }
            if is_phone_shape(&groups, has_sep) {
                return true;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// A digit-grouping is phone-shaped when it is a bare 10-digit run, an 11
/// digit run starting with the `1` country code, or — with separators —
/// 10/11 digits ending in a 4-digit group (US 3-3-4 / 3-4-4 / 1-3-3-4
/// shapes). The ending-4 rule is what keeps `2026-08-20 10:00:00` from
/// reading as a phone number.
fn is_phone_shape(groups: &[String], has_sep: bool) -> bool {
    let total: usize = groups.iter().map(|g| g.len()).sum();
    if total < 10 || total > 11 {
        return false;
    }
    if groups.is_empty() {
        return false;
    }
    if !has_sep {
        // bare run: the single group is everything
        return total == 10 || (total == 11 && groups[0].starts_with('1'));
    }
    // NANP area codes are at most 3 digits (or the 1 country-code group,
    // handled below); a 4-digit lead like `1234-56-7890` is not phone-shaped
    if groups[0].len() > 3 || groups.iter().any(|g| g.is_empty()) {
        return false;
    }
    if groups.last().unwrap().len() != 4 {
        return false;
    }
    total == 10 || (total == 11 && groups[0] == "1")
}

/// US street-address pattern (plan §12.1): a house number (bare digits,
/// ≤ 6) followed within the next four words by a recognized street suffix.
fn find_address(text: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "street", "st", "avenue", "ave", "road", "rd", "boulevard", "blvd",
        "lane", "ln", "drive", "dr", "court", "ct", "place", "pl", "way",
        "circle", "cir", "terrace", "parkway", "pkwy", "trail", "highway",
        "hwy", "crossing",
    ];
    let words: Vec<&str> = text.split_whitespace().collect();
    for (pos, word) in words.iter().enumerate() {
        if word.len() > 6 || !word.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        for w in &words[pos + 1..std::cmp::min(pos + 5, words.len())] {
            if SUFFIXES.contains(&w.to_lowercase().as_str()) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Forget ladder (plan §12.2)
// ---------------------------------------------------------------------------

/// Why the user is forgetting (plan §12.2). Mirrors the reference's
/// reason ladder, minus `duplicate` (memorabilia handles duplicates at
/// ingestion, not via forget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetReason {
    /// Sensitive data: hard delete + permanent tombstone.
    Private,
    /// Wrong fact: permanent suppression + tombstone (the row remains as
    /// evidence it was corrected; the text is never relearned).
    Wrong,
    /// Superseded: time-bounded suppression, no tombstone (it may legally
    /// come back as current fact).
    Outdated,
}

impl ForgetReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ForgetReason::Private => "private",
            ForgetReason::Wrong => "wrong",
            ForgetReason::Outdated => "outdated",
        }
    }
}

/// Tombstone granularity guard (plan §12.2). A tombstone is recorded only
/// when the deleted text clears `tombstone_min_chars` **or** contains a
/// high-entropy token: a digit run of at least `tombstone_digit_run`
/// digits, or any PII-pattern hit (emails, credentials, SSNs … already
/// carry their own identifying structure). Short generic phrases
/// ("call me tomorrow") are hard-deleted but **not** tombstoned, so
/// universal conversational patterns are not poisoned.
pub fn should_tombstone(text: &str, config: &Config) -> bool {
    let norm = normalize_text(text);
    if norm.chars().count() >= config.tombstone_min_chars {
        return true;
    }
    if !scan_pii(text).is_empty() {
        return true;
    }
    let mut run = 0u32;
    for ch in norm.chars() {
        if ch.is_ascii_digit() {
            run += 1;
            if (run as usize) >= config.tombstone_digit_run {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Engine entry point: delete / forget
// ---------------------------------------------------------------------------

impl Engine {
    /// Delete / forget the user-described content (plan §12.2).
    ///
    /// Resolution: the phrase is embedded with the engine's **real**
    /// embedder (never the lexical fallback — a different vector space
    /// would just miss), and every ACTIVE chunk whose normalized text
    /// matches exactly, or whose vector cosine in that same space is ≥
    /// `forget_match_threshold`, is affected.
    ///
    /// * `private`: one transaction removes each chunk's vector row, its
    ///   chunk row (FK cascades take the support links, dispute edges, and
    ///   outbox rows), applies the §7.4 orphan rule to any proposition
    ///   left without active support, and records permanent content +
    ///   document tombstones (granularity-guarded) so re-ingestion of the
    ///   same text is skipped forever.
    /// * `wrong`: permanent suppression + guarded content/document
    ///   tombstones; rows remain (evidence of correction, never recall-able
    ///   while suppressed).
    /// * `outdated`: `expires_at = now + outdated_suppression_days`
    ///   suppression, no tombstone.
    ///
    /// Returns the number of chunks affected. Soft-fail throughout:
    /// embed/DB errors are logged and yield 0, never a hard failure
    /// (claude.md principle 9). Audit rows record the reason and a
    /// structural detail — never the text or any PII value.
    pub async fn forget(&self, phrase: &str, reason: ForgetReason) -> u64 {
        let norm = normalize_text(phrase);
        if norm.is_empty() {
            tracing::warn!("forget: empty phrase after normalization -- ignoring");
            self.audit_forget(reason, 0).await;
            return 0;
        }
        let (query, tag) = match self.embedder.embed(&norm).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("forget: embedding the phrase failed ({e}); nothing forgotten");
                self.audit_forget(reason, 0).await;
                return 0;
            }
        };
        let active = match self.db.list_chunks_by_status("active").await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("forget: listing active chunks failed ({e}); nothing forgotten");
                self.audit_forget(reason, 0).await;
                return 0;
            }
        };

        // exact normalized-text match first (deterministic), then the
        // similarity floor in the space the embedder actually produced
        let mut matched = active
            .iter()
            .filter(|c| normalize_text(&c.content) == norm)
            .cloned()
            .collect::<Vec<_>>();
        let hits = self
            .vectors
            .search(&query, &tag, active.len().max(1))
            .await;
        if let Ok(hits) = hits {
            for (id, cos) in hits {
                if (cos as f64) < self.config.forget_match_threshold {
                    continue;
                }
                if matched.iter().any(|m| m.chunk_id == id) {
                    continue;
                }
                if let Some(c) = active.iter().find(|c| c.chunk_id == id) {
                    matched.push(c.clone());
                }
            }
        } else if let Err(e) = hits {
            tracing::warn!("forget: vector search failed ({e}); continuing with exact matches");
        }
        if matched.is_empty() {
            self.audit_forget(reason, 0).await;
            return 0;
        }

        let affected = matched.len() as u64;
        let (now, now_str) = timestamp_now();
        let config = self.config.clone();
        let db = &self.db;
        let vectors = self.vectors.clone();
        let now_str = now_str.clone();
        let expiry_opt = match reason {
            ForgetReason::Outdated => Some(
                (now + Duration::days(config.outdated_suppression_days as i64))
                    .format(TIMESTAMP_FMT)
                    .to_string(),
            ),
            _ => None,
        };

        let result = db
            .run_in_transaction(|| async move {
                let mut affected_nodes: Vec<String> = Vec::new();

                match reason {
                    ForgetReason::Private => {
                        for c in &matched {
                            affected_nodes
                                .extend(db.list_nodes_supported_by_chunk(&c.chunk_id).await?);
                            vectors
                                .remove(&c.chunk_id)
                                .await
                                .map_err(Error::Internal)?;
                        }
                        for c in &matched {
                            db.delete_chunk(&c.chunk_id).await?;
                        }
                        // §7.4 orphan rule for propositions the delete
                        // stripped of all active support. (Confidence
                        // recomputation for surviving propositions belongs
                        // to the math/extraction phases.)
                        for node in affected_nodes.iter().collect::<Vec<_>>() {
                            if db.count_active_sources(node).await? == 0 {
                                if let Some(p) = db.get_proposition(node).await? {
                                    if p.importance == "high" || p.importance == "unknown" {
                                        db.archive_proposition(node, &now_str).await?;
                                    } else {
                                        db.delete_proposition(node).await?;
                                    }
                                }
                            }
                        }
                        // tombstones (granularity-guarded), content + document
                        for c in &matched {
                            if should_tombstone(&c.content, &config) {
                                db.insert_tombstone(&Tombstone {
                                    text_hash: c.content_hash.clone(),
                                    kind: "content".into(),
                                    permanent: true,
                                    created_at: now_str.clone(),
                                })
                                .await?;
                                db.insert_tombstone(&Tombstone {
                                    text_hash: c.document_hash.clone(),
                                    kind: "document".into(),
                                    permanent: true,
                                    created_at: now_str.clone(),
                                })
                                .await?;
                            }
                        }
                    }
                    ForgetReason::Wrong => {
                        for c in &matched {
                            db.insert_suppression(&Suppression {
                                chunk_id: c.chunk_id.clone(),
                                reason: "wrong".into(),
                                permanent: true,
                                expires_at: None,
                                created_at: now_str.clone(),
                            })
                            .await?;
                            if should_tombstone(&c.content, &config) {
                                db.insert_tombstone(&Tombstone {
                                    text_hash: c.content_hash.clone(),
                                    kind: "content".into(),
                                    permanent: true,
                                    created_at: now_str.clone(),
                                })
                                .await?;
                                db.insert_tombstone(&Tombstone {
                                    text_hash: c.document_hash.clone(),
                                    kind: "document".into(),
                                    permanent: true,
                                    created_at: now_str.clone(),
                                })
                                .await?;
                            }
                        }
                    }
                    ForgetReason::Outdated => {
                        let expiry = expiry_opt
                            .as_ref()
                            .expect("expiry computed for Outdated above");
                        for c in &matched {
                            db.insert_suppression(&Suppression {
                                chunk_id: c.chunk_id.clone(),
                                reason: "outdated".into(),
                                permanent: false,
                                expires_at: Some(expiry.clone()),
                                created_at: now_str.clone(),
                            })
                            .await?;
                        }
                    }
                }
                Ok(())
            })
            .await;

        match result {
            Ok(()) => {
                self.audit_forget(reason, affected).await;
                affected
            }
            Err(e) => {
                tracing::warn!("forget: transaction failed ({e}); nothing forgotten");
                self.audit_forget(reason, 0).await;
                0
            }
        }
    }

    /// Forget one memory item by its proposition/item id — the Settings fact
    /// browser's delete action. Same permanent-suppression + tombstone path as
    /// `forget(.., ForgetReason::Wrong)`, but scoped to the item's own active
    /// supporting chunks rather than resolved from a phrase (the browser
    /// already holds the exact id, so there's nothing to embed and match).
    /// Suppressed chunks can't be recalled, and their text is tombstoned so
    /// extraction can't silently relearn it. Returns the number of supporting
    /// chunks suppressed (0 if the item is unknown or not active). Soft-fail:
    /// any DB error logs and yields 0 (claude.md principle 9).
    pub async fn forget_item(&self, node_id: &str) -> u64 {
        match self.db.get_proposition(node_id).await {
            Ok(Some(p)) if p.status == "active" => {}
            Ok(_) => return 0,
            Err(e) => {
                tracing::warn!("forget_item: lookup failed for {node_id} ({e}); nothing forgotten");
                return 0;
            }
        }
        let matched = match self.db.list_active_supporting_chunks(node_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("forget_item: listing supporters failed ({e}); nothing forgotten");
                self.audit_forget(ForgetReason::Wrong, 0).await;
                return 0;
            }
        };
        if matched.is_empty() {
            self.audit_forget(ForgetReason::Wrong, 0).await;
            return 0;
        }
        let affected = matched.len() as u64;
        let (_, now_str) = timestamp_now();
        let config = self.config.clone();
        let db = &self.db;
        let result = db
            .run_in_transaction(|| async move {
                for c in &matched {
                    db.insert_suppression(&Suppression {
                        chunk_id: c.chunk_id.clone(),
                        reason: "wrong".into(),
                        permanent: true,
                        expires_at: None,
                        created_at: now_str.clone(),
                    })
                    .await?;
                    if should_tombstone(&c.content, &config) {
                        db.insert_tombstone(&Tombstone {
                            text_hash: c.content_hash.clone(),
                            kind: "content".into(),
                            permanent: true,
                            created_at: now_str.clone(),
                        })
                        .await?;
                        db.insert_tombstone(&Tombstone {
                            text_hash: c.document_hash.clone(),
                            kind: "document".into(),
                            permanent: true,
                            created_at: now_str.clone(),
                        })
                        .await?;
                    }
                }
                Ok(())
            })
            .await;
        match result {
            Ok(()) => {
                self.audit_forget(ForgetReason::Wrong, affected).await;
                affected
            }
            Err(e) => {
                tracing::warn!("forget_item: transaction failed ({e}); nothing forgotten");
                self.audit_forget(ForgetReason::Wrong, 0).await;
                0
            }
        }
    }

    /// Category/reason-only audit for a forget call (claude.md principle 6):
    /// the event name carries the reason, the detail a structural count —
    /// never the text. Best-effort: an audit failure never changes the
    /// forget outcome.
    async fn audit_forget(&self, reason: ForgetReason, matched: u64) {
        let entry = forget_audit_entry(reason, matched);
        if let Err(e) = self.db.insert_audit(&entry).await {
            tracing::warn!("forget: audit insert failed ({e})");
        }
    }
}

fn forget_audit_entry(reason: ForgetReason, matched: u64) -> AuditEntry {
    let (_, created_at) = timestamp_now();
    AuditEntry {
        id: uuid::Uuid::new_v4().to_string(),
        event: format!("deleted:{}", reason.as_str()),
        category: None,
        detail: Some(if matched == 0 {
            "unresolved: no matching active chunk".into()
        } else {
            format!("chunks={matched}")
        }),
        created_at,
    }
}

fn timestamp_now() -> (DateTime<Utc>, String) {
    let now = Utc::now();
    (now, now.format(TIMESTAMP_FMT).to_string())
}

fn audit_pii_row(category: &str, created_at: &str) -> AuditEntry {
    AuditEntry {
        id: uuid::Uuid::new_v4().to_string(),
        event: "rejected:pii".into(),
        category: Some(category.to_string()),
        detail: None,
        created_at: created_at.to_string(),
    }
}

/// Audit one `rejected:pii` row per detected category, **category only** —
/// never the value (claude.md principle 6). Used by the Stage 0 gate
/// (learn pipeline). Best-effort: an audit failure never changes the
/// rejection decision.
pub async fn audit_pii_rejection(
    db: &crate::store::Db,
    categories: &[String],
    created_at: &str,
) {
    for cat in categories {
        let entry = audit_pii_row(cat, created_at);
        if let Err(e) = db.insert_audit(&entry).await {
            tracing::warn!("pii gate: audit insert failed ({e})");
        }
    }
}
