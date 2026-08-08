//! Domain routing. Cross-domain is a *routing* decision (multiply by a
//! factor), not deletion -- unlike the old bleed's near-zero cross-domain
//! weight.

use crate::store::beliefs::Belief;
use crate::vector::ops;

/// Cosine similarity a candidate belief's embedding must clear against the
/// query for its domain to be inferred as the query's domain.
const DOMAIN_INFER_THRESHOLD: f64 = 0.5;

/// Infer the current turn's domain from the single most-similar
/// domain-tagged belief, if similarity clears a modest bar. No separate
/// domain-centroid/clustering system exists (the plan's original design
/// envisioned one; building it is a substantial new feature, not a bug fix)
/// -- this is deliberately the simplest thing that could actually work:
/// "what domain is this message most like" = "the domain of whichever
/// existing belief most resembles it". `None` (no domain inferred, or an
/// empty/degenerate query) makes every belief cross-domain-neutral rather
/// than wrongly forcing a domain.
pub fn infer_query_domain(beliefs: &[Belief], query: &[f32]) -> Option<String> {
    if ops::norm(query) < 1e-12 {
        return None;
    }
    let mut best: Option<(f64, &str)> = None;
    for b in beliefs {
        let Some(d) = b.domain.as_deref() else { continue };
        let cos = ops::cosine(&b.embedding, query);
        if cos >= DOMAIN_INFER_THRESHOLD && best.as_ref().map(|(c, _)| cos > *c).unwrap_or(true) {
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
}
