//! The overnight pass (docs/MEMORY_OVERNIGHT.md §3–§4): turn the day's
//! screened captures into candidates, decide which topics have recurred, and
//! promote only those into memory for extraction. Cheapest work first, so
//! whatever the window leaves undone is the most expensive.
//!
//! The host decides *when* (the window, the user being away): the pass asks
//! [`PassControl::keep_going`] between items and returns as soon as it says
//! no, leaving everything else for the next run.

use std::collections::{HashMap, HashSet};

use chrono::Duration as ChronoDuration;
use serde::Serialize;

use crate::core;
use crate::engine::Engine;
use crate::learn::paragraphs::group_chunks;
use crate::learn::INITIAL_DECAY_CLASS;
use crate::store::chunks::Chunk;
use crate::store::documents::Document;
use crate::store::intake::{Candidate, Capture};
use crate::store::{decode_embedding, encode_embedding};
use crate::text::{citation_date, normalize_text, sha256_hex, slug};

/// Candidates that never recur are deleted after this long, unextracted.
pub const CANDIDATE_MAX_AGE_DAYS: i64 = 30;
/// Two candidates (or a candidate and a memory chunk) are the same topic at
/// this cosine or above.
pub const RECURRENCE_SIMILARITY: f32 = 0.80;
/// Two search queries ask for the same thing at this cosine or above.
pub const QUERY_SIMILARITY: f32 = 0.80;
/// A cluster promotes at this score.
pub const PROMOTE_AT: f64 = 2.0;
/// Points for each further similar search, per turn.
pub const SEARCH_POINTS: f64 = 0.5;
/// Candidates per promoted cluster written to memory.
pub const REPRESENTATIVES: usize = 2;
/// Most waiting candidates considered per night (newest first); the
/// clustering is all-pairs.
pub const CLUSTER_WINDOW: i64 = 3000;
/// Screen-log rows are kept this long, for labelling.
pub const SCREEN_LOG_MAX_AGE_DAYS: i64 = 180;

const CAPTURE_BATCH: i64 = 20;
const QUERY_BATCH: i64 = 200;
const STORE_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// The host's say over a running pass.
pub trait PassControl: Send + Sync {
    /// `false` stops the pass after the item in hand (the window ended, or
    /// the user is back at the machine).
    fn keep_going(&self) -> bool;
    /// The current time, ISO-8601 UTC with a literal `Z`.
    fn now(&self) -> String;
}

/// A pass control that never stops, at a fixed time (tests, "process now"
/// in hosts that bound the pass some other way).
pub struct FixedClock(pub String);

impl PassControl for FixedClock {
    fn keep_going(&self) -> bool {
        true
    }
    fn now(&self) -> String {
        self.0.clone()
    }
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OvernightReport {
    pub unscreened_dropped: u64,
    pub captures_chunked: u64,
    pub candidates_added: u64,
    pub clusters_promoted: u64,
    pub chunks_promoted: u64,
    pub candidates_merged: u64,
    pub candidates_expired: u64,
    pub extraction_attempted: u64,
    pub extraction_succeeded: u64,
    pub extraction_failed: u64,
    pub propositions_written: u64,
    /// Every stage ran to completion (nothing left for the next run).
    pub finished: bool,
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// [`cosine`] for the rest of the crate.
pub(crate) fn cosine_pub(a: &[f32], b: &[f32]) -> f32 {
    cosine(a, b)
}

fn find(parent: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while parent[r] != r {
        r = parent[r];
    }
    let mut i = i;
    while parent[i] != r {
        let next = parent[i];
        parent[i] = r;
        i = next;
    }
    r
}

fn days_before(now: chrono::NaiveDateTime, days: i64) -> String {
    (now - ChronoDuration::days(days))
        .format(STORE_TIMESTAMP_FORMAT)
        .to_string()
}

/// A waiting candidate with its vector decoded.
struct Live {
    c: Candidate,
    v: Vec<f32>,
}

impl Engine {
    /// One overnight pass. Soft-fail: errors are logged and the report says
    /// what was done.
    pub async fn overnight_pass(&self, ctl: &dyn PassControl) -> OvernightReport {
        let mut r = OvernightReport::default();
        let now = ctl.now();
        let Some(now_dt) = core::parse_utc(&now) else {
            tracing::warn!("overnight_pass: unparseable now ({now}); pass skipped");
            return r;
        };

        // 1. Housekeeping: unscreened captures past their wait, old queries,
        //    old screen-log rows, candidates that never recurred.
        let unscreened_cutoff =
            days_before(now_dt, crate::learn::capture::UNSCREENED_MAX_AGE_DAYS);
        r.unscreened_dropped = self
            .db
            .drop_unscreened_before(&unscreened_cutoff)
            .await
            .unwrap_or(0);
        let candidate_cutoff = days_before(now_dt, CANDIDATE_MAX_AGE_DAYS);
        let _ = self.db.delete_queries_before(&candidate_cutoff).await;
        let _ = self
            .db
            .delete_screen_log_before(&days_before(now_dt, SCREEN_LOG_MAX_AGE_DAYS))
            .await;
        r.candidates_expired = self
            .db
            .expire_candidates_before(&candidate_cutoff)
            .await
            .unwrap_or(0);

        // 2. Screened captures → chunks → candidates.
        loop {
            if !ctl.keep_going() {
                return r;
            }
            let batch = match self.db.list_captures("screened", CAPTURE_BATCH).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("overnight_pass: listing captures failed ({e})");
                    return r;
                }
            };
            if batch.is_empty() {
                break;
            }
            for c in batch {
                if !ctl.keep_going() {
                    return r;
                }
                match self.candidates_from_capture(&c, &ctl.now()).await {
                    Some(n) => {
                        r.captures_chunked += 1;
                        r.candidates_added += n;
                    }
                    // The embedder failed: stop rather than spin on it.
                    None => return r,
                }
            }
        }

        // 3. Embed the search queries recorded since the last pass.
        loop {
            if !ctl.keep_going() {
                return r;
            }
            let batch = match self
                .db
                .list_unembedded_queries(self.embedder.model_tag(), QUERY_BATCH)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("overnight_pass: listing queries failed ({e})");
                    return r;
                }
            };
            if batch.is_empty() {
                break;
            }
            let mut progressed = false;
            for q in batch {
                match self.embedder.embed(&q.query).await {
                    Ok((v, tag)) => {
                        if tag != self.embedder.model_tag() {
                            // Fallback space: leave it for a later pass
                            // rather than storing a vector no candidate can
                            // be compared with.
                            continue;
                        }
                        let _ = self
                            .db
                            .set_query_embedding(q.id, &encode_embedding(&v), &tag)
                            .await;
                        progressed = true;
                    }
                    Err(e) => {
                        tracing::warn!("overnight_pass: query embedding failed ({e})");
                        break;
                    }
                }
            }
            if !progressed {
                break;
            }
        }

        // 4. Recurrence: cluster, score, promote.
        if !ctl.keep_going() {
            return r;
        }
        if !self.promote(ctl, now_dt, &mut r).await {
            return r;
        }

        // 5. Extraction on what was promoted (and anything still pending),
        //    then maintenance.
        if !ctl.keep_going() {
            return r;
        }
        let now = ctl.now();
        self.maintenance_tick(&now).await;
        self.restore_unextracted_batch(&now).await;
        loop {
            if !ctl.keep_going() {
                return r;
            }
            let d = self.drain_extraction(&ctl.now()).await;
            r.extraction_attempted += d.attempted as u64;
            r.extraction_succeeded += d.succeeded as u64;
            r.extraction_failed += d.failed as u64;
            r.propositions_written += d.propositions_written as u64;
            // Nothing ready (failed chunks wait out their backoff), or every
            // attempt failed (the model is down): either way, done for now.
            if d.attempted == 0 || (d.succeeded == 0 && d.skipped_suppressed == 0) {
                break;
            }
        }
        r.finished = true;
        r
    }

    /// Chunk one screened capture into candidates. `None` when the embedder
    /// failed (the capture stays screened for the next pass); otherwise how
    /// many candidates were added.
    async fn candidates_from_capture(&self, c: &Capture, now: &str) -> Option<u64> {
        let mut added = 0u64;
        for text in group_chunks(&c.kept()) {
            let norm = normalize_text(&text);
            if norm.is_empty() {
                continue;
            }
            let hash = sha256_hex(&norm);
            let known = async {
                Ok::<bool, crate::error::Error>(
                    self.db.has_tombstone(&hash).await?
                        || self.db.get_chunk_by_content_hash(&hash).await?.is_some()
                        || self.db.has_candidate_hash(&hash).await?,
                )
            };
            match known.await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("overnight_pass: candidate dedup failed ({e})");
                    return None;
                }
            }
            let (v, tag) = match self.embedder.embed(&norm).await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!("overnight_pass: embedding failed ({e})");
                    return None;
                }
            };
            let cand = Candidate {
                candidate_id: format!("cand_{}", &hash[..24]),
                capture_id: c.capture_id.clone(),
                session_id: c.session_id.clone(),
                turn_id: c.turn_id,
                content: norm,
                content_hash: hash,
                embedding: encode_embedding(&v),
                embedding_model: tag,
                pinned: c.pinned,
                status: "waiting".into(),
                created_at: now.to_string(),
                decided_at: None,
            };
            match self.db.insert_candidate(&cand).await {
                Ok(true) => added += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("overnight_pass: candidate write failed ({e})");
                    return None;
                }
            }
        }
        if let Err(e) = self.db.close_capture(&c.capture_id, "processed").await {
            tracing::warn!("overnight_pass: closing capture failed ({e})");
        }
        Some(added)
    }

    /// Cluster the waiting candidates and promote the clusters that have
    /// recurred. `false` when the host stopped the pass part-way.
    async fn promote(
        &self,
        ctl: &dyn PassControl,
        now_dt: chrono::NaiveDateTime,
        r: &mut OvernightReport,
    ) -> bool {
        let waiting = match self.db.list_waiting_candidates(CLUSTER_WINDOW).await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("overnight_pass: listing candidates failed ({e})");
                return true;
            }
        };
        let live: Vec<Live> = waiting
            .into_iter()
            .map(|c| {
                let v = decode_embedding(&c.embedding);
                Live { c, v }
            })
            .collect();
        let n = live.len();
        let mut parent: Vec<usize> = (0..n).collect();
        for i in 0..n {
            for j in (i + 1)..n {
                if live[i].c.embedding_model == live[j].c.embedding_model
                    && cosine(&live[i].v, &live[j].v) >= RECURRENCE_SIMILARITY
                {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a != b {
                        parent[a] = b;
                    }
                }
            }
        }
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            clusters.entry(root).or_default().push(i);
        }
        // Deterministic order: by each cluster's first candidate id.
        let mut clusters: Vec<Vec<usize>> = clusters.into_values().collect();
        clusters.sort_by(|a, b| live[a[0]].c.candidate_id.cmp(&live[b[0]].c.candidate_id));

        let queries: Vec<(String, i64, Vec<f32>, String)> = self
            .db
            .list_queries_since(&days_before(now_dt, CANDIDATE_MAX_AGE_DAYS))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|q| {
                let v = decode_embedding(q.embedding.as_deref()?);
                Some((q.session_id, q.turn_id, v, q.embedding_model?))
            })
            .collect();

        for members in clusters {
            if !ctl.keep_going() {
                return false;
            }
            let score = self.cluster_score(&live, &members, &queries).await;
            let pinned = members.iter().any(|&i| live[i].c.pinned);
            if !pinned && score < PROMOTE_AT {
                continue;
            }
            self.promote_cluster(&live, &members, &ctl.now(), r).await;
        }
        true
    }

    /// The recurrence score of one cluster (docs/MEMORY_OVERNIGHT.md §4).
    async fn cluster_score(
        &self,
        live: &[Live],
        members: &[usize],
        queries: &[(String, i64, Vec<f32>, String)],
    ) -> f64 {
        // +1 per distinct chat (a capture with no session is its own chat).
        let chats: HashSet<String> = members
            .iter()
            .map(|&i| {
                live[i]
                    .c
                    .session_id
                    .clone()
                    .unwrap_or_else(|| format!("capture:{}", live[i].c.capture_id))
            })
            .collect();
        let mut score = chats.len() as f64;

        // +0.5 per further turn whose search resembles one from the
        // cluster's own turns.
        let own_turns: HashSet<(String, i64)> = members
            .iter()
            .filter_map(|&i| Some((live[i].c.session_id.clone()?, live[i].c.turn_id?)))
            .collect();
        let own_queries: Vec<&(String, i64, Vec<f32>, String)> = queries
            .iter()
            .filter(|q| own_turns.contains(&(q.0.clone(), q.1)))
            .collect();
        if !own_queries.is_empty() {
            let mut other_turns: HashSet<(String, i64)> = HashSet::new();
            for q in queries {
                let turn = (q.0.clone(), q.1);
                if own_turns.contains(&turn) || other_turns.contains(&turn) {
                    continue;
                }
                if own_queries
                    .iter()
                    .any(|o| o.3 == q.3 && cosine(&o.2, &q.2) >= QUERY_SIMILARITY)
                {
                    other_turns.insert(turn);
                }
            }
            score += SEARCH_POINTS * other_turns.len() as f64;
        }

        // +1 when memory already holds the topic. Only looked up when it
        // could change the decision: it is a vector search per member.
        if score < PROMOTE_AT {
            for &i in members.iter().take(3) {
                let hit = self
                    .vectors
                    .search(&live[i].v, &live[i].c.embedding_model, 1)
                    .await
                    .ok()
                    .and_then(|h| h.into_iter().next());
                if hit.is_some_and(|(_, cos)| cos >= RECURRENCE_SIMILARITY) {
                    score += 1.0;
                    break;
                }
            }
        }
        score
    }

    /// Write a cluster's most central candidates into memory as pending
    /// chunks; mark the rest merged.
    async fn promote_cluster(
        &self,
        live: &[Live],
        members: &[usize],
        now: &str,
        r: &mut OvernightReport,
    ) {
        // Centrality: mean cosine to the other members.
        let mut ranked: Vec<(usize, f32)> = members
            .iter()
            .map(|&i| {
                let others = members.len().saturating_sub(1).max(1) as f32;
                let sum: f32 = members
                    .iter()
                    .filter(|&&j| j != i)
                    .map(|&j| cosine(&live[i].v, &live[j].v))
                    .sum();
                (i, sum / others)
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| live[a.0].c.candidate_id.cmp(&live[b.0].c.candidate_id))
        });
        // Prefer representatives from different documents.
        let mut chosen: Vec<usize> = Vec::new();
        let mut captures: HashSet<&str> = HashSet::new();
        for &(i, _) in &ranked {
            if chosen.len() >= REPRESENTATIVES {
                break;
            }
            if captures.insert(live[i].c.capture_id.as_str()) {
                chosen.push(i);
            }
        }
        for &(i, _) in &ranked {
            if chosen.len() >= REPRESENTATIVES {
                break;
            }
            if !chosen.contains(&i) {
                chosen.push(i);
            }
        }

        let mut written = 0u64;
        for &i in &chosen {
            if self.store_promoted(&live[i].c, &live[i].v, now).await {
                written += 1;
            }
            let _ = self
                .db
                .set_candidate_status(&live[i].c.candidate_id, "promoted", now)
                .await;
        }
        for &i in members {
            if !chosen.contains(&i) {
                let _ = self
                    .db
                    .set_candidate_status(&live[i].c.candidate_id, "merged", now)
                    .await;
                r.candidates_merged += 1;
            }
        }
        r.clusters_promoted += 1;
        r.chunks_promoted += written;
    }

    /// Write one promoted candidate into memory exactly as `ingest` writes a
    /// chunk: document row, source seed, provenance cluster, pending
    /// extraction. `false` when it was skipped or failed.
    async fn store_promoted(&self, c: &Candidate, v: &[f32], now: &str) -> bool {
        let cap = match self.db.get_capture(&c.capture_id).await {
            Ok(Some(cap)) => cap,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!("overnight_pass: capture lookup failed ({e})");
                return false;
            }
        };
        match (
            self.db.has_tombstone(&c.content_hash).await,
            self.db.has_tombstone(&cap.document_hash).await,
            self.db.get_chunk_by_content_hash(&c.content_hash).await,
        ) {
            (Ok(false), Ok(false), Ok(None)) => {}
            (Ok(_), Ok(_), Ok(_)) => return false,
            _ => {
                tracing::warn!("overnight_pass: promotion lookups failed");
                return false;
            }
        }

        let (tier, prior) = self.capture_tier(&cap);
        let seed = (prior * cap.intent_factor).clamp(0.0, 1.0);
        if let Err(e) = self
            .db
            .seed_source(&cap.source_entity, tier, prior, seed, now)
            .await
        {
            tracing::warn!("overnight_pass: source seed failed ({e})");
            return false;
        }
        let reliability = match self.db.get_source(&cap.source_entity).await {
            Ok(Some(s)) => s.reliability,
            Ok(None) => seed,
            Err(e) => {
                tracing::warn!("overnight_pass: source read failed ({e})");
                return false;
            }
        };
        let citation = format!(
            "{}: {} / {}",
            cap.source_type,
            cap.source_name,
            citation_date(&cap.captured_at)
        );
        let own_cluster = slug(&format!("{}: {}", cap.source_type, cap.source_name));
        let cluster_id = self
            .resolve_cluster(v, &c.embedding_model, &own_cluster)
            .await;

        let db = &self.db;
        let vectors = self.vectors.clone();
        let result = db
            .run_in_transaction(|| async move {
                let existing = db.get_document(&cap.document_hash).await?;
                let count = match existing {
                    Some(d) => d.chunk_count,
                    None => {
                        db.insert_document(&Document {
                            document_hash: cap.document_hash.clone(),
                            source_entity: cap.source_entity.clone(),
                            source_type: cap.source_type.clone(),
                            source_name: cap.source_name.clone(),
                            captured_at: cap.captured_at.clone(),
                            intent_factor: cap.intent_factor,
                            chunk_count: 0,
                            created_at: now.to_string(),
                        })
                        .await?;
                        0
                    }
                };
                let chunk = Chunk {
                    chunk_id: format!("c_{}", &c.content_hash[..24]),
                    content: c.content.clone(),
                    content_hash: c.content_hash.clone(),
                    document_hash: cap.document_hash.clone(),
                    source_entity: cap.source_entity.clone(),
                    source_reliability: reliability,
                    provenance_cluster_id: cluster_id,
                    cluster_citation: citation,
                    status: "active".into(),
                    extraction_status: "pending".into(),
                    extraction_error_at: None,
                    embedding_model: c.embedding_model.clone(),
                    decay_class: INITIAL_DECAY_CLASS.into(),
                    anchor_at: now.to_string(),
                    urgency_expires_at: None,
                    reinforcement_count: 0,
                    grounding_count: 0,
                    archived_at: None,
                    created_at: now.to_string(),
                };
                db.insert_chunk(&chunk).await?;
                vectors
                    .upsert(&chunk.chunk_id, v, &chunk.embedding_model)
                    .await
                    .map_err(crate::error::Error::Internal)?;
                db.set_document_chunk_count(&cap.document_hash, count + 1)
                    .await?;
                Ok(())
            })
            .await;
        match result {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("overnight_pass: promotion write failed ({e})");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::capture::CaptureInput;
    use crate::learn::IngestInput;

    async fn engine() -> Engine {
        crate::learn::capture::tests::engine().await
    }

    /// A long, distinctive paragraph about one topic; `variant` changes a few
    /// words so two documents differ (different hashes) but stay similar.
    fn doc(topic: &str, variant: &str) -> String {
        let p = format!(
            "{topic} matters because harbour engineers learned over centuries that sloped rubble \
             faces absorb wave energy far better than vertical walls, which reflect it and scour \
             the seabed at their foot. {variant} The lesson recurs in every exposed bay, from \
             medieval breakwaters to modern container ports, and it explains why so many new \
             projects return to rock armour after concrete experiments fail in storms."
        );
        format!("{p}\n\n{p} Again, {variant}")
    }

    fn input(content: String, session: &str, turn: i64, captured_at: &str) -> CaptureInput {
        CaptureInput {
            ingest: IngestInput {
                content,
                source_type: "Scraped".into(),
                source_name: "example.org".into(),
                source_entity: "example.org".into(),
                captured_at: captured_at.into(),
                intent: None,
            },
            session_id: Some(session.into()),
            turn_id: Some(turn),
            pinned: false,
        }
    }

    const T0: &str = "2026-11-01T10:00:00Z";
    const NIGHT: &str = "2026-11-02T01:00:00Z";

    async fn active_chunks(e: &Engine) -> usize {
        e.db.list_chunks_by_status("active").await.unwrap().len()
    }

    #[tokio::test]
    async fn one_chat_waits_and_is_never_recalled() {
        let e = engine().await;
        e.capture(&input(doc("Breakwater design", "first"), "s1", 1, T0)).await;
        let r = e.overnight_pass(&FixedClock(NIGHT.into())).await;
        assert_eq!(r.candidates_added, 1);
        assert_eq!(r.clusters_promoted, 0);
        assert_eq!(active_chunks(&e).await, 0);
        assert!(r.finished);
    }

    #[tokio::test]
    async fn two_chats_promote_once_per_cluster() {
        let e = engine().await;
        e.capture(&input(doc("Breakwater design", "first"), "s1", 1, T0)).await;
        e.capture(&input(doc("Breakwater design", "second"), "s2", 4, T0)).await;
        e.capture(&input(doc("Breakwater design", "third"), "s2", 5, T0)).await;
        let r = e.overnight_pass(&FixedClock(NIGHT.into())).await;
        assert_eq!(r.clusters_promoted, 1);
        assert_eq!(r.chunks_promoted, REPRESENTATIVES as u64);
        assert_eq!(r.candidates_merged, 1);
        assert_eq!(active_chunks(&e).await, REPRESENTATIVES);
    }

    #[tokio::test]
    async fn repeated_searches_lift_a_single_chat_topic() {
        let e = engine().await;
        e.capture(&input(doc("Breakwater design", "first"), "s1", 1, T0)).await;
        let q = vec!["why sloped breakwaters beat vertical sea walls".to_string()];
        e.record_demand("s1", 1, &q, T0).await;
        // Two later turns (same chat) asking the same thing.
        e.record_demand("s1", 9, &q, T0).await;
        e.record_demand("s1", 12, &q, T0).await;
        // Several queries in one turn count once.
        e.record_demand("s1", 12, &["why sloped breakwaters beat vertical sea walls?".into()], T0)
            .await;
        let r = e.overnight_pass(&FixedClock(NIGHT.into())).await;
        assert_eq!(r.clusters_promoted, 1, "1 chat + 2 × 0.5 = 2");
    }

    #[tokio::test]
    async fn pinned_documents_promote_alone() {
        let e = engine().await;
        let mut i = input(doc("Breakwater design", "first"), "s1", 1, T0);
        i.pinned = true;
        e.capture(&i).await;
        let r = e.overnight_pass(&FixedClock(NIGHT.into())).await;
        assert_eq!(r.clusters_promoted, 1);
    }

    #[tokio::test]
    async fn candidates_expire_after_thirty_days() {
        let e = engine().await;
        e.capture(&input(doc("Breakwater design", "first"), "s1", 1, T0)).await;
        e.overnight_pass(&FixedClock(NIGHT.into())).await;
        let r = e
            .overnight_pass(&FixedClock("2026-12-05T01:00:00Z".into()))
            .await;
        assert_eq!(r.candidates_expired, 1);
        assert!(e.db.list_waiting_candidates(10).await.unwrap().is_empty());
    }

    struct Stop;
    impl PassControl for Stop {
        fn keep_going(&self) -> bool {
            false
        }
        fn now(&self) -> String {
            NIGHT.into()
        }
    }

    #[tokio::test]
    async fn the_host_can_stop_a_pass() {
        let e = engine().await;
        e.capture(&input(doc("Breakwater design", "first"), "s1", 1, T0)).await;
        let r = e.overnight_pass(&Stop).await;
        assert!(!r.finished);
        assert_eq!(r.captures_chunked, 0);
        assert_eq!(e.db.list_captures("screened", 10).await.unwrap().len(), 1);
    }
}
