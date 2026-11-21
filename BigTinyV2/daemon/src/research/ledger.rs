//! The ledger's typed entries, how repeats are recognised, and what every
//! entry has become — all derived from append-only rows on read.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::research::{EntryRow, LinkRow, TagRow};

// ---------------------------------------------------------------- payloads

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    /// One self-contained claim.
    pub claim: String,
    /// The passage or data that supports it, quoted or closely paraphrased.
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Who asserts it, when the source relays someone else's claim
    /// (e.g. "the plaintiff's complaint"). Empty when the source states it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub attribution: String,
    /// Whether this came from pursuing a tangent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tangent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Source {
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub date: String,
    /// A downloaded copy, when the source is a document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
}

fn default_priority() -> u8 {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    pub text: String,
    /// 1 (most important) to 3.
    #[serde(default = "default_priority")]
    pub priority: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Lead {
    /// What to look into next.
    pub text: String,
    /// What in the findings suggests it.
    pub why: String,
    /// True when it lies outside the contract's coverage but could change or
    /// enrich the answer.
    #[serde(default)]
    pub tangent: bool,
    #[serde(default = "default_priority")]
    pub priority: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeadEnd {
    /// What was tried.
    pub query: String,
    /// Why it gave nothing useful.
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Entity {
    pub name: String,
    /// person, organization, work, place, case, or other.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Defect {
    /// unsupported, gap, contradiction, or scope.
    pub kind: String,
    /// What it concerns: an entry (`L12`), a section id, or a coverage cell id.
    pub target: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_query: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Search the web and read pages.
    Search,
    /// Read documents already found (cached paths or document ids in `refs`).
    Read,
    /// Count, compute or compare figures.
    Compute,
}

/// One frontier item: a structured brief for one delegate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    pub kind: TaskKind,
    /// What to find out.
    pub objective: String,
    /// What to bring back, and in how much detail.
    pub output_format: String,
    /// Which tools and sources to try, and what to avoid.
    pub guidance: String,
    /// Where to stop: scope, dates, how deep.
    pub boundaries: String,
    /// For `read`: cached paths or document ids. For `search`: optional URLs.
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub tangent: bool,
}

/// An entry's payload, by kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Finding(Finding),
    Source(Source),
    Question(Question),
    Lead(Lead),
    DeadEnd(DeadEnd),
    Entity(Entity),
    Defect(Defect),
    Task(Task),
}

impl Body {
    pub fn kind(&self) -> &'static str {
        match self {
            Body::Finding(_) => "finding",
            Body::Source(_) => "source",
            Body::Question(_) => "question",
            Body::Lead(_) => "lead",
            Body::DeadEnd(_) => "dead_end",
            Body::Entity(_) => "entity",
            Body::Defect(_) => "defect",
            Body::Task(_) => "task",
        }
    }

    pub fn parse(kind: &str, payload: &str) -> Result<Body, String> {
        fn p<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, String> {
            serde_json::from_str(s).map_err(|e| e.to_string())
        }
        Ok(match kind {
            "finding" => Body::Finding(p(payload)?),
            "source" => Body::Source(p(payload)?),
            "question" => Body::Question(p(payload)?),
            "lead" => Body::Lead(p(payload)?),
            "dead_end" => Body::DeadEnd(p(payload)?),
            "entity" => Body::Entity(p(payload)?),
            "defect" => Body::Defect(p(payload)?),
            "task" => Body::Task(p(payload)?),
            other => return Err(format!("unknown entry kind {other:?}")),
        })
    }

    pub fn payload_json(&self) -> String {
        let v = match self {
            Body::Finding(x) => serde_json::to_string(x),
            Body::Source(x) => serde_json::to_string(x),
            Body::Question(x) => serde_json::to_string(x),
            Body::Lead(x) => serde_json::to_string(x),
            Body::DeadEnd(x) => serde_json::to_string(x),
            Body::Entity(x) => serde_json::to_string(x),
            Body::Defect(x) => serde_json::to_string(x),
            Body::Task(x) => serde_json::to_string(x),
        };
        v.unwrap_or_else(|_| "{}".into())
    }

    /// The key an exact repeat shares. Leads, defects and tasks are judgments
    /// made in context and are never merged mechanically.
    pub fn norm_key(&self) -> Option<String> {
        match self {
            // A web page by its normalized URL; a document by its path or id.
            Body::Source(s) => {
                norm_url(&s.url).or_else(|| (!s.url.trim().is_empty()).then(|| text_key(&s.url)))
            }
            Body::Finding(f) => Some(text_key(&f.claim)),
            Body::Question(q) => Some(text_key(&q.text)),
            Body::DeadEnd(d) => Some(text_key(&d.query)),
            Body::Entity(e) => Some(text_key(&format!("{} {}", e.kind, e.name))),
            Body::Lead(_) | Body::Defect(_) | Body::Task(_) => None,
        }
    }

    /// One line, for projections.
    pub fn headline(&self) -> String {
        match self {
            Body::Finding(f) => {
                let who = if f.attribution.is_empty() {
                    String::new()
                } else {
                    format!(" (per {})", f.attribution)
                };
                format!("{}{who}", clip(&f.claim, 220))
            }
            Body::Source(s) => {
                let title = if s.title.is_empty() { &s.url } else { &s.title };
                format!("{} <{}>", clip(title, 120), s.url)
            }
            Body::Question(q) => clip(&q.text, 200),
            Body::Lead(l) => format!("{} (because {})", clip(&l.text, 160), clip(&l.why, 120)),
            Body::DeadEnd(d) => format!("{}: {}", clip(&d.query, 120), clip(&d.note, 100)),
            Body::Entity(e) => format!("{} [{}] {}", e.name, e.kind, clip(&e.note, 120)),
            Body::Defect(d) => format!("{} on {}: {}", d.kind, d.target, clip(&d.description, 180)),
            Body::Task(t) => format!("{:?}: {}", t.kind, clip(&t.objective, 180)).to_lowercase(),
        }
    }
}

/// `s` cut to `max` characters on a char boundary, with an ellipsis.
pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------- normalizing

/// Lowercase words, punctuation dropped, whitespace collapsed.
pub fn norm_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn text_key(s: &str) -> String {
    let digest = Sha256::digest(norm_text(s).as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

const TRACKING_PARAMS: [&str; 6] = ["fbclid", "gclid", "mc_cid", "mc_eid", "ref", "ref_src"];

/// A URL reduced to what identifies the page: no scheme, no `www.`, no
/// fragment, no tracking parameters, no trailing slash. `None` for text that
/// is not an http(s) URL.
pub fn norm_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let path = url.path().trim_end_matches('/');
    let query: Vec<String> = url
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !k.starts_with("utm_") && !TRACKING_PARAMS.contains(&k.as_str())
        })
        .map(|(k, v)| {
            if v.is_empty() {
                k.into_owned()
            } else {
                format!("{k}={v}")
            }
        })
        .collect();
    let mut out = format!("{host}{path}");
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    Some(out)
}

/// The registrable-ish host of a URL (`www.` dropped), for counting
/// independent sources.
pub fn domain_of(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

// ---------------------------------------------------------------- the view

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub seq: i64,
    pub wave: i64,
    pub body: Body,
}

/// What an entry has become, derived from links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    /// Repeats the entry with this seq.
    Duplicate(i64),
    /// Replaced by a newer finding.
    Superseded,
    /// Contradicted by another finding, or contradicting one.
    Disputed,
    /// A question or lead a finding answers.
    Answered,
    /// A question or lead a task has taken up, not yet answered.
    Pursued,
    /// A defect a finding addresses.
    Resolved,
    Open,
}

/// The whole ledger of one run, with its derived state.
#[derive(Debug, Default, Clone)]
pub struct Ledger {
    entries: BTreeMap<i64, Entry>,
    links: Vec<LinkRow>,
    cells: HashMap<i64, BTreeSet<String>>,
    sections: HashMap<i64, BTreeSet<String>>,
    /// Rows whose payload no longer parses; reported, never fatal.
    pub unreadable: Vec<i64>,
}

impl Ledger {
    pub fn build(entries: Vec<EntryRow>, links: Vec<LinkRow>, tags: Vec<TagRow>) -> Ledger {
        let mut ledger = Ledger::default();
        for row in entries {
            match Body::parse(&row.kind, &row.payload) {
                Ok(body) => {
                    ledger.entries.insert(
                        row.seq,
                        Entry {
                            seq: row.seq,
                            wave: row.wave,
                            body,
                        },
                    );
                }
                Err(_) => ledger.unreadable.push(row.seq),
            }
        }
        for t in tags {
            let map = if t.tag_kind == "cell" {
                &mut ledger.cells
            } else {
                &mut ledger.sections
            };
            map.entry(t.seq).or_default().insert(t.tag);
        }
        ledger.links = links;
        ledger
    }

    pub fn get(&self, seq: i64) -> Option<&Entry> {
        self.entries.get(&seq)
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Entry> + 'a {
        self.entries.values().filter(move |e| e.body.kind() == kind)
    }

    fn incoming<'a>(&'a self, seq: i64, kind: &'a str) -> impl Iterator<Item = i64> + 'a {
        self.links
            .iter()
            .filter(move |l| l.to_seq == seq && l.kind == kind)
            .map(|l| l.from_seq)
    }

    fn outgoing<'a>(&'a self, seq: i64, kind: &'a str) -> impl Iterator<Item = i64> + 'a {
        self.links
            .iter()
            .filter(move |l| l.from_seq == seq && l.kind == kind)
            .map(|l| l.to_seq)
    }

    pub fn status(&self, seq: i64) -> Status {
        let Some(entry) = self.entries.get(&seq) else {
            return Status::Open;
        };
        if let Some(of) = self.outgoing(seq, "duplicates").next() {
            return Status::Duplicate(of);
        }
        match &entry.body {
            Body::Finding(_) => {
                if self.incoming(seq, "supersedes").next().is_some() {
                    Status::Superseded
                } else if self.incoming(seq, "disputes").next().is_some()
                    || self.outgoing(seq, "disputes").next().is_some()
                {
                    Status::Disputed
                } else {
                    Status::Active
                }
            }
            Body::Question(_) | Body::Lead(_) => {
                if self.incoming(seq, "answers").next().is_some() {
                    Status::Answered
                } else if self.incoming(seq, "pursues").next().is_some() {
                    Status::Pursued
                } else {
                    Status::Open
                }
            }
            Body::Defect(_) => {
                if self.incoming(seq, "addresses").next().is_some() {
                    Status::Resolved
                } else {
                    Status::Open
                }
            }
            _ => Status::Active,
        }
    }

    /// A finding that can be used in the deliverable: neither a duplicate
    /// nor superseded. Disputed findings stay usable — both sides are shown.
    pub fn is_usable_finding(&self, seq: i64) -> bool {
        matches!(self.get(seq).map(|e| &e.body), Some(Body::Finding(_)))
            && matches!(self.status(seq), Status::Active | Status::Disputed)
    }

    /// Source entries a finding rests on: `cites` links, plus the source
    /// whose URL matches the finding's own `source_url`.
    pub fn sources_of(&self, finding: i64) -> Vec<i64> {
        let mut out: BTreeSet<i64> = self.outgoing(finding, "cites").collect();
        if let Some(Body::Finding(f)) = self.get(finding).map(|e| &e.body) {
            if let Some(key) = f.source_url.as_deref().and_then(norm_url) {
                for e in self.of_kind("source") {
                    if let Body::Source(s) = &e.body {
                        if norm_url(&s.url).as_deref() == Some(key.as_str()) {
                            out.insert(e.seq);
                        }
                    }
                }
            }
        }
        out.into_iter().collect()
    }

    /// Distinct domains among a finding's sources, its duplicates' included:
    /// a claim three sites repeat is corroborated, one site saying it thrice
    /// is not.
    pub fn independent_sources(&self, finding: i64) -> usize {
        let mut group: Vec<i64> = vec![finding];
        group.extend(self.incoming(finding, "duplicates"));
        let mut domains = BTreeSet::new();
        for seq in group {
            for src in self.sources_of(seq) {
                if let Some(Body::Source(s)) = self.get(src).map(|e| &e.body) {
                    if let Some(d) = domain_of(&s.url) {
                        domains.insert(d);
                    }
                }
            }
            if let Some(Body::Finding(f)) = self.get(seq).map(|e| &e.body) {
                if let Some(d) = f.source_url.as_deref().and_then(domain_of) {
                    domains.insert(d);
                }
            }
        }
        domains.len()
    }

    pub fn cells_of(&self, seq: i64) -> impl Iterator<Item = &String> {
        self.cells.get(&seq).into_iter().flatten()
    }

    pub fn sections_of(&self, seq: i64) -> impl Iterator<Item = &String> {
        self.sections.get(&seq).into_iter().flatten()
    }

    /// Usable findings placed in a coverage cell.
    pub fn findings_in_cell(&self, cell: &str) -> Vec<i64> {
        self.of_kind("finding")
            .filter(|e| self.is_usable_finding(e.seq))
            .filter(|e| self.cells.get(&e.seq).is_some_and(|c| c.contains(cell)))
            .map(|e| e.seq)
            .collect()
    }

    /// Usable findings placed in a section.
    pub fn findings_in_section(&self, section: &str) -> Vec<i64> {
        self.of_kind("finding")
            .filter(|e| self.is_usable_finding(e.seq))
            .filter(|e| {
                self.sections
                    .get(&e.seq)
                    .is_some_and(|c| c.contains(section))
            })
            .map(|e| e.seq)
            .collect()
    }

    /// Pairs of findings that contradict each other.
    pub fn disputes(&self) -> Vec<(i64, i64)> {
        self.links
            .iter()
            .filter(|l| l.kind == "disputes")
            .map(|l| (l.from_seq, l.to_seq))
            .collect()
    }

    /// Tangent leads a usable finding answers: the tangents that paid off.
    pub fn paid_off_tangents(&self) -> Vec<(i64, Vec<i64>)> {
        self.of_kind("lead")
            .filter(|e| matches!(&e.body, Body::Lead(l) if l.tangent))
            .filter_map(|e| {
                let answers: Vec<i64> = self
                    .incoming(e.seq, "answers")
                    .filter(|f| self.is_usable_finding(*f))
                    .collect();
                (!answers.is_empty()).then_some((e.seq, answers))
            })
            .collect()
    }

    /// Entries a lead was read out of.
    pub fn derived_from(&self, lead: i64) -> Vec<i64> {
        self.outgoing(lead, "derived_from").collect()
    }

    /// Leads or questions a task acts on.
    pub fn pursued_by(&self, task: i64) -> Vec<i64> {
        self.outgoing(task, "pursues").collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn row(seq: i64, wave: i64, body: &Body) -> EntryRow {
        EntryRow {
            seq,
            wave,
            kind: body.kind().into(),
            payload: body.payload_json(),
            norm_key: body.norm_key(),
        }
    }

    pub(crate) fn link(from: i64, to: i64, kind: &str) -> LinkRow {
        LinkRow {
            from_seq: from,
            to_seq: to,
            kind: kind.into(),
            reason: String::new(),
            wave: 1,
        }
    }

    pub(crate) fn finding(claim: &str, url: &str) -> Body {
        Body::Finding(Finding {
            claim: claim.into(),
            evidence: "quoted".into(),
            source_url: Some(url.into()),
            attribution: String::new(),
            tangent: false,
        })
    }

    pub(crate) fn source(url: &str) -> Body {
        Body::Source(Source {
            url: url.into(),
            title: String::new(),
            date: String::new(),
            cached_path: None,
            document_id: None,
        })
    }

    pub(crate) fn lead(text: &str, tangent: bool) -> Body {
        Body::Lead(Lead {
            text: text.into(),
            why: "seen in L1".into(),
            tangent,
            priority: 2,
        })
    }

    #[test]
    fn urls_normalize_to_the_page() {
        assert_eq!(
            norm_url("https://www.Example.com/a/b/?utm_source=x&id=3&fbclid=9#frag").as_deref(),
            Some("example.com/a/b?id=3")
        );
        assert_eq!(
            norm_url("http://example.com/a"),
            norm_url("https://example.com/a/")
        );
        assert_eq!(norm_url("not a url"), None);
        assert_eq!(norm_url("file:///c:/x.pdf"), None);
    }

    #[test]
    fn claims_match_across_case_and_punctuation() {
        let a = finding("Asimov's \"Robbie\" appeared in 1940.", "https://a.com");
        let b = finding("asimovs robbie appeared in 1940", "https://b.com");
        assert_ne!(a.norm_key(), b.norm_key(), "the apostrophe splits a word");
        let c = finding("Asimov's  Robbie appeared, in 1940!", "https://c.com");
        let d = finding("asimov s robbie appeared in 1940", "https://d.com");
        assert_eq!(c.norm_key(), d.norm_key());
        assert!(lead("x", false).norm_key().is_none());
    }

    #[test]
    fn payloads_round_trip() {
        for body in [
            finding("c", "https://a.com"),
            source("https://a.com"),
            lead("l", true),
        ] {
            let back = Body::parse(body.kind(), &body.payload_json()).unwrap();
            assert_eq!(back, body);
        }
        assert!(Body::parse("bogus", "{}").is_err());
    }

    #[test]
    fn status_and_corroboration_derive_from_links() {
        let rows = vec![
            row(
                1,
                1,
                &finding("Robbie is from 1940", "https://isfdb.org/t/1"),
            ),
            row(2, 1, &source("https://isfdb.org/t/1")),
            row(
                3,
                1,
                &finding(
                    "Robbie dates to 1940",
                    "https://en.wikipedia.org/wiki/Robbie",
                ),
            ),
            row(4, 1, &finding("Robbie is from 1939", "https://other.net/x")),
            row(5, 1, &lead("Read the 1950 collection", true)),
            row(
                6,
                2,
                &finding("I, Robot collects Robbie", "https://isfdb.org/t/2"),
            ),
            row(
                7,
                2,
                &Body::Question(Question {
                    text: "When?".into(),
                    priority: 1,
                }),
            ),
            row(8, 2, &finding("Old claim", "https://x.org")),
            row(9, 2, &finding("New claim", "https://x.org")),
        ];
        let links = vec![
            link(1, 2, "cites"),
            link(3, 1, "duplicates"),
            link(4, 1, "disputes"),
            link(6, 5, "answers"),
            link(5, 1, "derived_from"),
            link(9, 8, "supersedes"),
        ];
        let tags = vec![TagRow {
            seq: 1,
            tag_kind: "cell".into(),
            tag: "magazines".into(),
            wave: 1,
        }];
        let l = Ledger::build(rows, links, tags);
        assert_eq!(l.status(3), Status::Duplicate(1));
        assert_eq!(l.status(1), Status::Disputed);
        assert_eq!(l.status(4), Status::Disputed);
        assert_eq!(l.status(8), Status::Superseded);
        assert_eq!(l.status(5), Status::Answered);
        assert_eq!(l.status(7), Status::Open);
        assert!(l.is_usable_finding(1));
        assert!(!l.is_usable_finding(3));
        assert!(!l.is_usable_finding(8));
        assert_eq!(l.sources_of(1), vec![2]);
        assert_eq!(
            l.independent_sources(1),
            2,
            "isfdb + the duplicate's wikipedia"
        );
        assert_eq!(l.findings_in_cell("magazines"), vec![1]);
        assert_eq!(l.paid_off_tangents(), vec![(5, vec![6])]);
        assert_eq!(l.derived_from(5), vec![1]);
        assert_eq!(l.disputes(), vec![(4, 1)]);
    }
}
