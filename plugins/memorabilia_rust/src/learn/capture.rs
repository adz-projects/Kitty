//! Daytime intake (docs/MEMORY_OVERNIGHT.md §1–§3): what a turn brought in is
//! privacy-checked, de-duplicated, split into paragraphs, pre-filtered and —
//! when the app's decision model is on and reachable — screened paragraph by
//! paragraph. Only the kept paragraphs are stored, in the capture buffer.
//! Nothing here embeds or calls an extraction model; that is the overnight
//! pass's job (`learn::overnight`).
//!
//! Soft-fail throughout, like `ingest`: every error is logged and the outcome
//! says what was written.

use chrono::Duration as ChronoDuration;

use crate::core;
use crate::engine::Engine;
use crate::learn::paragraphs::{occurrence_counts, prefilter, split_paragraphs, Paragraph};
use crate::learn::{tier_and_prior_for, AttachmentIntent, IngestInput, INTENT_EVIDENCE};
use crate::privacy::{audit_pii_rejection, pii_categories};
use crate::store::intake::{Capture, ScreenLogEntry};
use crate::text::sha256_hex;
use crate::traits::ScreenVerdict;

/// How long a capture may wait for the decision model before it is dropped.
pub const UNSCREENED_MAX_AGE_DAYS: i64 = 3;
/// After this many screened paragraphs with none kept, the rest of the
/// document is dropped unscreened.
pub const EARLY_EXIT_AFTER: usize = 10;
/// Screened captures the daytime retry handles per call.
pub const SCREEN_PENDING_BATCH: i64 = 3;

const STORE_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// A turn's document, plus where in the conversation it came from.
#[derive(Debug, Clone)]
pub struct CaptureInput {
    pub ingest: IngestInput,
    pub session_id: Option<String>,
    /// The turn's user-message rowid: identifies the turn for the demand
    /// signal.
    pub turn_id: Option<i64>,
    /// The user marked it "remember this": promoted without waiting for
    /// recurrence.
    pub pinned: bool,
}

/// What [`Engine::capture`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureOutcome {
    /// Rejected by the privacy check (categories only, never values).
    pub rejected_pii: Vec<String>,
    /// Already captured, already in memory, or forgotten.
    pub already_seen: bool,
    /// Stored raw, waiting for the decision model.
    pub unscreened: bool,
    pub paragraphs_total: usize,
    pub paragraphs_kept: usize,
    /// Nothing survived the screen; the capture is recorded as dropped.
    pub dropped: bool,
}

/// The outcome of screening one document's paragraphs.
enum DocScreen {
    /// The screen is on but did not answer: try again later.
    Unavailable,
    Done {
        kept: Vec<String>,
        total: usize,
        log: Vec<ScreenLogEntry>,
    },
}

/// The text the screen sees for one paragraph: the paragraph with the
/// document's identity and its section heading.
pub fn screen_state(source_type: &str, source_name: &str, p: &Paragraph) -> String {
    let mut s = format!("Document: {source_name} ({source_type})\n");
    if let Some(h) = &p.heading {
        s.push_str(&format!("Section: {h}\n"));
    }
    s.push('\n');
    s.push_str(p.text.trim());
    s
}

fn scores_json(scores: &[(String, f32)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = scores
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::json!((*v * 1000.0).round() / 1000.0)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

impl Engine {
    /// Daytime intake of one document (docs/MEMORY_OVERNIGHT.md). Never
    /// embeds and never calls an extraction model.
    pub async fn capture(&self, input: &CaptureInput) -> CaptureOutcome {
        let mut outcome = CaptureOutcome::default();
        let ing = &input.ingest;
        let content = ing.content.trim();
        if content.is_empty() {
            return outcome;
        }

        // Privacy check before anything is written (plan §12.1).
        let categories = pii_categories(self.pii_classifier.as_ref(), content).await;
        if !categories.is_empty() {
            audit_pii_rejection(&self.db, &categories, &ing.captured_at).await;
            outcome.rejected_pii = categories;
            return outcome;
        }

        // Whole-document dedup: forgotten, already in memory, or already
        // captured.
        let doc_hash = sha256_hex(content);
        let seen = async {
            Ok::<bool, crate::error::Error>(
                self.db.has_tombstone(&doc_hash).await?
                    || self.db.get_document(&doc_hash).await?.is_some()
                    || self.db.has_capture(&doc_hash).await?,
            )
        };
        match seen.await {
            Ok(true) => {
                outcome.already_seen = true;
                return outcome;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("capture: dedup lookup failed ({e}); document skipped");
                return outcome;
            }
        }

        let intent_factor = ing
            .intent
            .map(AttachmentIntent::factor)
            .unwrap_or(INTENT_EVIDENCE)
            .clamp(0.0, 1.0);
        let capture_id = format!("cap_{}", &doc_hash[..24]);
        let mut capture = Capture {
            capture_id: capture_id.clone(),
            document_hash: doc_hash,
            session_id: input.session_id.clone(),
            turn_id: input.turn_id,
            source_type: ing.source_type.clone(),
            source_name: ing.source_name.clone(),
            source_entity: ing.source_entity.clone(),
            intent_factor,
            captured_at: ing.captured_at.clone(),
            status: "unscreened".into(),
            raw_text: None,
            kept_paragraphs: None,
            paragraphs_total: 0,
            paragraphs_kept: 0,
            pinned: input.pinned,
            screened_at: None,
        };

        match self
            .screen_document(&capture_id, &ing.source_type, &ing.source_name, content, &ing.captured_at)
            .await
        {
            DocScreen::Unavailable => {
                capture.raw_text = Some(content.to_string());
                outcome.unscreened = true;
                if let Err(e) = self.db.insert_capture(&capture).await {
                    tracing::warn!("capture: write failed ({e}); document skipped");
                    return CaptureOutcome::default();
                }
            }
            DocScreen::Done { kept, total, log } => {
                outcome.paragraphs_total = total;
                outcome.paragraphs_kept = kept.len();
                outcome.dropped = kept.is_empty();
                capture.status = if kept.is_empty() { "dropped" } else { "screened" }.into();
                capture.kept_paragraphs =
                    Some(serde_json::to_string(&kept).unwrap_or_else(|_| "[]".into()));
                capture.paragraphs_total = total as i64;
                capture.paragraphs_kept = kept.len() as i64;
                capture.screened_at = Some(ing.captured_at.clone());
                if let Err(e) = self.db.insert_capture(&capture).await {
                    tracing::warn!("capture: write failed ({e}); document skipped");
                    return CaptureOutcome::default();
                }
                self.write_screen_log(&log).await;
            }
        }
        outcome
    }

    async fn write_screen_log(&self, log: &[ScreenLogEntry]) {
        for e in log {
            if let Err(err) = self.db.insert_screen_log(e).await {
                tracing::warn!("capture: screen-log write failed ({err})");
                return;
            }
        }
    }

    /// Split, pre-filter and screen one document.
    async fn screen_document(
        &self,
        capture_id: &str,
        source_type: &str,
        source_name: &str,
        text: &str,
        now: &str,
    ) -> DocScreen {
        let paragraphs = split_paragraphs(text);
        let seen = occurrence_counts(&paragraphs);
        let screen = self.paragraph_screen.get().cloned();
        let enabled = match &screen {
            Some(s) => s.enabled().await,
            None => false,
        };
        let question_set = match (&screen, enabled) {
            (Some(s), true) => Some(s.question_set().await),
            _ => None,
        };
        let entry = |p: &Paragraph, scores: Option<String>, verdict: String| ScreenLogEntry {
            capture_id: capture_id.to_string(),
            source_type: source_type.to_string(),
            source_name: source_name.to_string(),
            heading: p.heading.clone(),
            text: p.text.clone(),
            question_set: question_set.clone(),
            scores,
            verdict,
            created_at: now.to_string(),
        };

        // Screen off (none configured / switched off): every non-prefiltered
        // paragraph is kept; no model calls, no batching.
        let Some(s) = screen.as_ref().filter(|_| enabled) else {
            let mut kept = Vec::new();
            let mut log = Vec::new();
            for p in &paragraphs {
                match prefilter(p, &seen) {
                    Some(reason) => log.push(entry(p, None, format!("prefilter:{}", reason.as_str()))),
                    None => {
                        kept.push(p.text.clone());
                        log.push(entry(p, None, "kept".into()));
                    }
                }
            }
            return DocScreen::Done { kept, total: paragraphs.len(), log };
        };

        // Classify once: prefiltered (free) vs screenable.
        let prefiltered: Vec<Option<String>> = paragraphs
            .iter()
            .map(|p| prefilter(p, &seen).map(|r| r.as_str().to_string()))
            .collect();
        let screen_indices: Vec<usize> =
            (0..paragraphs.len()).filter(|&i| prefiltered[i].is_none()).collect();

        // Screen the screenable paragraphs in document order, in groups of
        // `batch_size` (one model call each). The early-exit rule still holds:
        // if the first EARLY_EXIT_AFTER screened paragraphs all fail, stop
        // (listing pages fail top to bottom). `verdicts` maps a paragraph index
        // to its verdict; `stopped_at` marks where the early exit halted, so
        // nothing at or after it is recorded — matching the old single `break`.
        let batch = s.batch_size().await.max(1);
        let mut verdicts: std::collections::HashMap<usize, ScreenVerdict> =
            std::collections::HashMap::new();
        let mut screened = 0usize;
        let mut any_kept = false;
        let mut stopped_at: Option<usize> = None;
        for chunk in screen_indices.chunks(batch) {
            if screened >= EARLY_EXIT_AFTER && !any_kept {
                stopped_at = chunk.first().copied();
                break;
            }
            let states: Vec<String> = chunk
                .iter()
                .map(|&i| screen_state(source_type, source_name, &paragraphs[i]))
                .collect();
            let refs: Vec<&str> = states.iter().map(String::as_str).collect();
            for (&i, v) in chunk.iter().zip(s.screen_batch(&refs).await) {
                match &v {
                    // The model is down: the whole document waits, unscreened.
                    ScreenVerdict::Unavailable => return DocScreen::Unavailable,
                    ScreenVerdict::Disabled => any_kept = true,
                    ScreenVerdict::Scored { keep, .. } => {
                        screened += 1;
                        any_kept |= *keep;
                    }
                }
                verdicts.insert(i, v);
            }
        }

        // Assemble kept + log in document order, stopping where the early exit
        // halted.
        let mut kept = Vec::new();
        let mut log = Vec::new();
        for (i, p) in paragraphs.iter().enumerate() {
            if stopped_at.is_some_and(|stop| i >= stop) {
                break;
            }
            match &prefiltered[i] {
                Some(reason) => log.push(entry(p, None, format!("prefilter:{reason}"))),
                None => match verdicts.get(&i) {
                    Some(ScreenVerdict::Disabled) => {
                        kept.push(p.text.clone());
                        log.push(entry(p, None, "kept".into()));
                    }
                    Some(ScreenVerdict::Scored { keep, scores }) => {
                        if *keep {
                            kept.push(p.text.clone());
                        }
                        let verdict = if *keep { "kept" } else { "dropped" };
                        log.push(entry(p, Some(scores_json(scores)), verdict.into()));
                    }
                    // Unavailable returns above; None only past the early exit.
                    Some(ScreenVerdict::Unavailable) | None => {}
                },
            }
        }
        DocScreen::Done { kept, total: paragraphs.len(), log }
    }

    /// Retry captures that were stored while the decision model was
    /// unreachable, oldest first; drop those older than
    /// [`UNSCREENED_MAX_AGE_DAYS`]. Stops at the first one the model still
    /// cannot answer. Returns how many were screened.
    pub async fn screen_pending(&self, now: &str) -> usize {
        let Some(now_dt) = core::parse_utc(now) else {
            return 0;
        };
        let cutoff = (now_dt - ChronoDuration::days(UNSCREENED_MAX_AGE_DAYS))
            .format(STORE_TIMESTAMP_FORMAT)
            .to_string();
        match self.db.drop_unscreened_before(&cutoff).await {
            Ok(n) if n > 0 => tracing::info!(dropped = n, "memorabilia: unscreened captures expired"),
            Ok(_) => {}
            Err(e) => tracing::warn!("screen_pending: expiry failed ({e})"),
        }
        let batch = match self.db.list_captures("unscreened", SCREEN_PENDING_BATCH).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("screen_pending: listing failed ({e})");
                return 0;
            }
        };
        let mut done = 0usize;
        for c in batch {
            let Some(raw) = c.raw_text.as_deref() else {
                let _ = self.db.close_capture(&c.capture_id, "dropped").await;
                continue;
            };
            match self
                .screen_document(&c.capture_id, &c.source_type, &c.source_name, raw, now)
                .await
            {
                DocScreen::Unavailable => break,
                DocScreen::Done { kept, total, log } => {
                    let status = if kept.is_empty() { "dropped" } else { "screened" };
                    if let Err(e) = self
                        .db
                        .set_capture_screened(&c.capture_id, status, &kept, total as i64, now)
                        .await
                    {
                        tracing::warn!("screen_pending: update failed ({e})");
                        break;
                    }
                    self.write_screen_log(&log).await;
                    done += 1;
                }
            }
        }
        done
    }

    /// Record the search queries one turn issued (the demand signal).
    pub async fn record_demand(&self, session_id: &str, turn_id: i64, queries: &[String], now: &str) {
        for q in queries {
            let q = q.trim();
            if q.is_empty() {
                continue;
            }
            if let Err(e) = self.db.insert_demand_query(session_id, turn_id, q, now).await {
                tracing::warn!("record_demand: write failed ({e})");
                return;
            }
        }
    }

    /// Forget's reach into the intake (plan §12.2): waiting candidates that
    /// match the phrase, and captures whose text contains it, are dropped so
    /// a forgotten fact cannot come back through a later promotion.
    pub(crate) async fn forget_pending(&self, norm: &str, query: &[f32], tag: &str, threshold: f64) {
        if let Ok(waiting) = self.db.list_waiting_candidates(i64::MAX).await {
            for c in waiting {
                let same = c.content == norm
                    || (c.embedding_model == tag
                        && crate::learn::overnight::cosine_pub(
                            &crate::store::decode_embedding(&c.embedding),
                            query,
                        ) as f64
                            >= threshold);
                if same {
                    let _ = self.db.delete_candidate(&c.candidate_id).await;
                }
            }
        }
        let needle = norm.to_lowercase();
        for status in ["unscreened", "screened"] {
            if let Ok(caps) = self.db.list_captures(status, i64::MAX).await {
                for c in caps {
                    let text = c.raw_text.clone().unwrap_or_else(|| c.kept().join(" "));
                    if crate::text::normalize_text(&text).to_lowercase().contains(&needle) {
                        let _ = self.db.close_capture(&c.capture_id, "dropped").await;
                    }
                }
            }
        }
    }

    /// Tier and prior for a capture's source, as `ingest` computes them.
    pub(crate) fn capture_tier(&self, c: &Capture) -> (&'static str, f64) {
        tier_and_prior_for(&self.reliability, &c.source_type, &c.source_name)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Config;
    use crate::embed::HashEmbedder;
    use crate::store::Db;
    use crate::traits::{MockChat, MockParagraphScreen, MemoryVectorIndex};
    use std::sync::Arc;

    pub(crate) async fn engine() -> Engine {
        let db = Db::open_in_memory().await.unwrap();
        Engine::new(
            Config::default(),
            db,
            Arc::new(MockChat { response: serde_json::json!({"propositions": [], "disputes": []}) }),
            Arc::new(HashEmbedder::new(Config::default().embedding_dim)),
            Arc::new(MemoryVectorIndex::new()),
        )
    }

    fn para(seed: &str) -> String {
        format!(
            "{seed} explains how harbour walls spread the force of storm waves along their length, \
             which is why engineers favour sloped rubble faces over vertical concrete in exposed bays, \
             and why so many modern ports have returned to rock armour after costly storm damage."
        )
    }

    fn input(content: String) -> CaptureInput {
        CaptureInput {
            ingest: IngestInput {
                content,
                source_type: "Scraped".into(),
                source_name: "example.org".into(),
                source_entity: "example.org".into(),
                captured_at: "2026-11-01T10:00:00Z".into(),
                intent: None,
            },
            session_id: Some("s1".into()),
            turn_id: Some(7),
            pinned: false,
        }
    }

    #[tokio::test]
    async fn without_a_screen_the_prefilter_decides() {
        let e = engine().await;
        let doc = format!("{}\n\nHome\nNews\nSport\nWeather\nAbout", para("This page"));
        let out = e.capture(&input(doc)).await;
        assert_eq!(out.paragraphs_kept, 1);
        assert!(!out.unscreened);
        let c = e.db.list_captures("screened", 10).await.unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].kept().len(), 1);
        assert!(c[0].raw_text.is_none());
    }

    #[tokio::test]
    async fn the_screen_keeps_only_what_it_passes_and_logs_everything() {
        let e = engine().await;
        e.set_paragraph_screen(Arc::new(MockParagraphScreen {
            enabled: true,
            fixed: None,
            keep_if: vec!["KEEP".into()],
        }));
        let doc = format!("{}\n\n{}", para("KEEP this"), para("Not that"));
        let out = e.capture(&input(doc)).await;
        assert_eq!((out.paragraphs_total, out.paragraphs_kept), (2, 1));
        let log = e.db.list_screen_log(10).await.unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.iter().any(|r| r.verdict == "dropped"));
        assert!(log.iter().all(|r| r.question_set.as_deref() == Some("mock")));
    }

    #[tokio::test]
    async fn an_unreachable_screen_holds_the_document_then_retries() {
        let e = engine().await;
        e.set_paragraph_screen(Arc::new(MockParagraphScreen {
            enabled: true,
            fixed: Some(ScreenVerdict::Unavailable),
            keep_if: vec![],
        }));
        let out = e.capture(&input(para("Held"))).await;
        assert!(out.unscreened);
        let held = e.db.list_captures("unscreened", 10).await.unwrap();
        assert_eq!(held.len(), 1);
        assert!(held[0].raw_text.is_some());
        // Still unreachable: nothing changes.
        assert_eq!(e.screen_pending("2026-11-01T11:00:00Z").await, 0);
        // Past three days: dropped.
        assert_eq!(e.screen_pending("2026-11-05T11:00:00Z").await, 0);
        assert_eq!(e.db.list_captures("dropped", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_document_is_captured_once() {
        let e = engine().await;
        assert!(!e.capture(&input(para("Once"))).await.already_seen);
        assert!(e.capture(&input(para("Once"))).await.already_seen);
    }

    #[tokio::test]
    async fn pii_rejects_before_anything_is_written() {
        let e = engine().await;
        let out = e
            .capture(&input(format!("{} Contact jane.doe@example.com today.", para("Mail"))))
            .await;
        assert!(!out.rejected_pii.is_empty());
        assert!(e.db.list_captures("screened", 10).await.unwrap().is_empty());
    }
}
