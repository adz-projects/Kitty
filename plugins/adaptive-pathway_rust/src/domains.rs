//! Domain routing. Cross-domain is a *routing* decision (multiply by a
//! factor), not deletion -- unlike the old bleed's near-zero cross-domain
//! weight.

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
