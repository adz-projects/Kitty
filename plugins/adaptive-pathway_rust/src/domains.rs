//! Domain routing. Cross-domain is a *routing* decision (multiply by a
//! factor), not deletion -- unlike the old bleed's near-zero cross-domain
//! weight.

use crate::store::beliefs::Belief;
use crate::vector::ops;

/// Cosine similarity a candidate belief's embedding must clear against the
/// query for its domain to be inferred as the query's domain (small-store raw fallback).
const DOMAIN_INFER_THRESHOLD: f64 = 0.5;

/// Centered-cosine similarity threshold used when there are enough beliefs (`>= 3`)
/// to estimate a meaningful corpus mean, avoiding anisotropic false positives.
const DOMAIN_INFER_CENTERED_THRESHOLD: f64 = 0.25;

/// Infer the current turn's domain from the single most-similar
/// domain-tagged belief, if similarity clears a modest bar. Uses centered
/// cosine when `beliefs.len() >= 3` and a matching-dimension corpus mean exists,
/// falling back to raw cosine on tiny stores (`< 3` beliefs). `None` (no domain
/// inferred, or an empty/degenerate query) makes every belief cross-domain-neutral
/// rather than wrongly forcing a domain.
pub fn infer_query_domain(beliefs: &[Belief], query: &[f32]) -> Option<String> {
    if ops::norm(query) < 1e-12 {
        return None;
    }
    let mean = if beliefs.len() >= 3 {
        crate::recall::corpus_mean(beliefs).filter(|m| m.len() == query.len())
    } else {
        None
    };
    let qc = mean.as_ref().map(|m| crate::recall::sub(query, m));
    let mut best: Option<(f64, &str)> = None;
    for b in beliefs {
        let Some(d) = b.domain.as_deref() else { continue };
        let (cos, threshold) = match (&qc, &mean) {
            (Some(qc), Some(m)) if m.len() == b.embedding.len() => (
                ops::cosine(qc, &crate::recall::sub(&b.embedding, m)),
                DOMAIN_INFER_CENTERED_THRESHOLD,
            ),
            _ => (ops::cosine(&b.embedding, query), DOMAIN_INFER_THRESHOLD),
        };
        if cos >= threshold && best.as_ref().map(|(c, _)| cos > *c).unwrap_or(true) {
            best = Some((cos, d));
        }
    }
    best.map(|(_, d)| d.to_string())
}

/// Cross-domain weight: same-domain = 1.0, cross-domain = 0.35.
pub fn domain_match(same_domain: bool) -> f64 {
    if same_domain {
        1.0
    } else {
        0.35
    }
}

/// Domain weight for recall: an explicit domain mismatch (`Some(q) != Some(b)`)
/// is down-weighted to `0.35`, while same-domain, domainless queries, and
/// domain-general beliefs (`belief == None`) stay neutral at `1.0`.
pub fn domain_weight(query: Option<&str>, belief: Option<&str>) -> f64 {
    match (query, belief) {
        (Some(q), Some(b)) if q != b => 0.35,
        _ => 1.0,
    }
}

/// Exact-domain equality. In practice domains are resolved against the
/// `domains` table; a belief with no domain always counts as cross-domain
/// against a query that names one (and vice versa kept neutral here).
pub fn same_domain(query: Option<&str>, belief: Option<&str>) -> bool {
    match (query, belief) {
        (Some(q), Some(b)) => q == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::beliefs::{Layer, Provenance};
    use chrono::Utc;

    fn belief(domain: Option<&str>, emb: Vec<f32>) -> Belief {
        let now = Utc::now();
        Belief {
            id: "b".into(),
            text: "x".into(),
            embedding: emb,
            confidence: 0.6,
            provenance: Provenance::DirectStatement,
            layer: Layer::Context,
            tested: true,
            domain: domain.map(|s| s.into()),
            tier: "context".into(),
            support_count: 1,
            distinct_sessions: 1,
            contradict_count: 0,
            pinned: false,
            last_confirmed_at: Some(now),
            consolidated_at: None,
            created_at: now,
            updated_at: now,
            session_id: None,
            embedding_model: crate::config::DEFAULT_EMBEDDING_MODEL.into(),
            kind: crate::store::beliefs::BeliefKind::Context,
        }
    }

    #[test]
    fn empty_query_infers_nothing() {
        let beliefs = vec![belief(Some("coding"), vec![1.0, 0.0])];
        assert_eq!(infer_query_domain(&beliefs, &[]), None);
    }

    #[test]
    fn infers_the_closest_domain_tagged_belief() {
        let beliefs = vec![
            belief(Some("coding"), vec![1.0, 0.0]),
            belief(Some("cooking"), vec![0.0, 1.0]),
        ];
        assert_eq!(infer_query_domain(&beliefs, &[1.0, 0.0]), Some("coding".to_string()));
        assert_eq!(infer_query_domain(&beliefs, &[0.0, 1.0]), Some("cooking".to_string()));
    }

    #[test]
    fn below_threshold_infers_nothing() {
        let beliefs = vec![belief(Some("coding"), vec![1.0, 0.0])];
        // Orthogonal query -- cosine 0.0, well under the 0.5 bar.
        assert_eq!(infer_query_domain(&beliefs, &[0.0, 1.0]), None);
    }

    #[test]
    fn domainless_beliefs_are_ignored() {
        let beliefs = vec![belief(None, vec![1.0, 0.0])];
        assert_eq!(infer_query_domain(&beliefs, &[1.0, 0.0]), None);
    }

    #[test]
    fn domain_weight_keeps_domainless_beliefs_neutral() {
        assert_eq!(domain_weight(Some("coding"), None), 1.0);
        assert_eq!(domain_weight(Some("coding"), Some("coding")), 1.0);
        assert_eq!(domain_weight(None, Some("coding")), 1.0);
        assert!((domain_weight(Some("coding"), Some("cooking")) - 0.35).abs() < 1e-12);
    }

    #[test]
    fn infer_query_domain_uses_centered_cosine_for_anisotropic_pools() {
        // All vectors share a strong common direction [0.9, ...], so raw cosine
        // against any query is >= 0.5, yet centered cosine distinguishes them.
        let beliefs = vec![
            belief(Some("coding"), vec![0.9, 0.4, 0.0]),
            belief(Some("cooking"), vec![0.9, -0.2, 0.3]),
            belief(Some("writing"), vec![0.9, -0.2, -0.3]),
        ];
        // Query is close to coding along the centered axis.
        assert_eq!(
            infer_query_domain(&beliefs, &[0.9, 0.4, 0.0]),
            Some("coding".to_string())
        );
        // Query orthogonal to all domain directions after centering -> None.
        assert_eq!(infer_query_domain(&beliefs, &[1.0, 0.0, 0.0]), None);
    }
}
