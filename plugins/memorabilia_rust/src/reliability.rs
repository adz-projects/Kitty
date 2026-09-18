//! Automatic source-reliability data (project-plan.md §4.2).
//!
//! `source_reliability` is never hand-coded. It is seeded at ingestion from
//! the tier prior (this module's data file) times the attachment intent
//! factor, then drifted automatically from outcomes. The tier table and the
//! reputable-domain set are **data files, not user config** — the user adds
//! nothing here.

use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Tier names, in descending `tier_prior` order (plan §4.2).
pub const TIER_PRIMARY: &str = "primary";
pub const TIER_ESTABLISHED: &str = "established";
pub const TIER_COMMUNITY: &str = "community";
pub const TIER_PERSONAL: &str = "personal";

#[derive(Debug, Clone, Deserialize)]
pub struct Tier {
    /// One of [`TIER_PRIMARY`], [`TIER_ESTABLISHED`], [`TIER_COMMUNITY`],
    /// [`TIER_PERSONAL`].
    pub name: String,
    /// The `tier_prior` this tier seeds reliability with (plan §4.2).
    pub prior: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReliabilityData {
    pub tiers: Vec<Tier>,
    /// Editorial-controlled major publishers (plan §4.2).
    pub reputable_domains: Vec<String>,
}

/// The tier table + reputable-domain set, baked into the binary at compile
/// time. `include_str!` embeds the file *contents* (not a path), so the
/// classifier never depends on the source tree being present at runtime — a
/// deployed or relocated binary carries its own copy. `data/reliability.yaml`
/// remains the single source of truth; this is that file, compiled in.
const EMBEDDED_RELIABILITY: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/reliability.yaml"));

impl Default for ReliabilityData {
    /// Parse-failure emergency only: reached solely if the compiled-in data
    /// (or an explicit override file) fails to deserialize, which a passing
    /// test suite rules out for the embedded copy. Degrades to these priors
    /// and an empty reputable set (everything but the structural hits →
    /// `personal`, the conservative side) instead of failing to construct.
    fn default() -> Self {
        Self {
            tiers: vec![
                Tier { name: TIER_PRIMARY.into(), prior: 0.95 },
                Tier { name: TIER_ESTABLISHED.into(), prior: 0.80 },
                Tier { name: TIER_COMMUNITY.into(), prior: 0.55 },
                Tier { name: TIER_PERSONAL.into(), prior: 0.35 },
            ],
            reputable_domains: Vec::new(),
        }
    }
}

impl ReliabilityData {
    /// Parse a data file from YAML text, normalizing it for lookup.
    fn parse(raw: &str) -> Result<Self> {
        let mut data: Self =
            serde_yaml::from_str(raw).map_err(|e| Error::Config(e.to_string()))?;
        // Lowercase the reputable set once here so `reputable_match` compares
        // against pre-normalized entries instead of re-lowercasing every
        // domain on every classification.
        for r in &mut data.reputable_domains {
            *r = r.to_lowercase();
        }
        Ok(data)
    }

    /// Load an explicit override data file from disk (tooling / tests). The
    /// default path is compiled in via [`Self::load_default`]; this exists for
    /// the rare case of pointing at an alternate file at runtime.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Self::parse(&raw)
    }

    /// Load the compiled-in default (`data/reliability.yaml`, embedded at
    /// build time — see [`EMBEDDED_RELIABILITY`]). No filesystem access.
    pub fn load_default() -> Result<Self> {
        Self::parse(EMBEDDED_RELIABILITY)
    }

    pub fn prior_for(&self, tier: &str) -> Option<f64> {
        self.tiers.iter().find(|t| t.name == tier).map(|t| t.prior)
    }

    /// Classify a bare host (e.g. `docs.python.org`) into a tier from
    /// structural/known signals only — never a per-source number (plan §4.2
    /// tier table):
    ///
    /// * `primary` — `.edu`/`.gov`, `*.github.io`
    /// * `established` — matched against `reputable_domains`
    /// * `community` — wikis, forums, Stack Overflow
    /// * `personal` — everything else (unknown domains default to personal)
    ///
    /// A bare `docs.*` prefix is deliberately **not** a primary signal: any
    /// host can name a subdomain `docs.` (`docs.attacker.com`), so it would
    /// grant the 0.95 prior on nothing. Real documentation on a known domain
    /// still lands in `established`/`community` via `reputable_domains` and
    /// drifts up from good outcomes.
    pub fn tier_for_domain(&self, domain: &str) -> &'static str {
        let d = domain.trim().trim_end_matches('/').to_lowercase();
        if d.ends_with(".edu") || d.ends_with(".gov") || d.ends_with(".github.io") {
            TIER_PRIMARY
        } else if self.reputable_match(&d) {
            TIER_ESTABLISHED
        } else if d == "stackoverflow.com"
            || d.ends_with(".stackoverflow.com")
            || d == "wikipedia.org"
            || d.ends_with(".wikipedia.org")
        {
            TIER_COMMUNITY
        } else {
            TIER_PERSONAL
        }
    }

    /// Suffix match so `news.nytimes.com` still hits the registered
    /// `nytimes.com` entry. `domain` arrives lowercased from
    /// [`Self::tier_for_domain`] and `reputable_domains` is lowercased once at
    /// load (see [`Self::parse`]), so no per-call allocation is needed here.
    fn reputable_match(&self, domain: &str) -> bool {
        self.reputable_domains
            .iter()
            .any(|r| domain == r || domain.ends_with(&format!(".{r}")))
    }
}
