//! §4.2 reliability data files: tier table + reputable-domain set. Data,
//! not user config — the user adds nothing here, and the priors are pinned
//! to the plan §4.2 tier table.

use memorabilia::reliability::{
    ReliabilityData, TIER_COMMUNITY, TIER_ESTABLISHED, TIER_PERSONAL, TIER_PRIMARY,
};

#[test]
fn default_data_file_loads_with_plan_section42_tiers() {
    let d = ReliabilityData::load_default().unwrap();
    assert_eq!(d.prior_for(TIER_PRIMARY), Some(0.95));
    assert_eq!(d.prior_for(TIER_ESTABLISHED), Some(0.80));
    assert_eq!(d.prior_for(TIER_COMMUNITY), Some(0.55));
    assert_eq!(d.prior_for(TIER_PERSONAL), Some(0.35));
    assert!(!d.reputable_domains.is_empty());
}

#[test]
fn tier_classification_follows_plan_section42_examples() {
    let d = ReliabilityData::load_default().unwrap();
    // primary: .edu/.gov, *.github.io
    assert_eq!(d.tier_for_domain("mit.edu"), TIER_PRIMARY);
    assert_eq!(d.tier_for_domain("usa.gov"), TIER_PRIMARY);
    assert_eq!(d.tier_for_domain("repo.github.io"), TIER_PRIMARY);
    // established: reputable-domain set, suffix-matched (case-insensitive)
    assert_eq!(d.tier_for_domain("nytimes.com"), TIER_ESTABLISHED);
    assert_eq!(d.tier_for_domain("news.nytimes.com"), TIER_ESTABLISHED);
    assert_eq!(d.tier_for_domain("NEWS.NYTIMES.COM"), TIER_ESTABLISHED);
    // community: wikis, forums, Stack Overflow
    assert_eq!(d.tier_for_domain("en.wikipedia.org"), TIER_COMMUNITY);
    assert_eq!(d.tier_for_domain("stackoverflow.com"), TIER_COMMUNITY);
    // personal: self-hosted blogs and unknown domains (the default)
    assert_eq!(d.tier_for_domain("myblog.wordpress.com"), TIER_PERSONAL);
    assert_eq!(d.tier_for_domain("unknown.example"), TIER_PERSONAL);
    // A bare `docs.*` prefix is NOT a trust signal: an arbitrary host that
    // names a subdomain `docs.` gets no prior boost.
    assert_eq!(d.tier_for_domain("docs.attacker.com"), TIER_PERSONAL);
    assert_eq!(d.tier_for_domain("docs.python.org"), TIER_PERSONAL);
}

#[test]
fn seeding_inputs_are_data_driven() {
    // §4.2: seed = clamp(tier_prior × intent_factor). The prior used at
    // seeding comes from the data file, never from a hand-coded per-source
    // number (claude.md, core principle 7).
    let d = ReliabilityData::load_default().unwrap();
    let prior = d.prior_for(d.tier_for_domain("nytimes.com")).unwrap();
    assert!((prior - 0.80).abs() < 1e-12);
    let prior = d.prior_for(d.tier_for_domain("whatever-unknown.net")).unwrap();
    assert!((prior - 0.35).abs() < 1e-12);
}
