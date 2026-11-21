//! What a step sees of the run: a bounded rendering of the contract and the
//! ledger, never a transcript. Each wave's planner starts on a fresh context
//! with only this, so nothing is lost to a rewritten summary and nothing
//! accumulates from earlier waves' tool output.

use crate::research::contract::{waves_for, Contract};
use crate::research::ledger::{Body, Ledger, Status};

/// Default character budget for a projection.
pub const DEFAULT_MAX_CHARS: usize = 24_000;

const MAX_LIST: usize = 25;
const MAX_NEW: usize = 80;

pub struct ProjectionInput<'a> {
    pub brief: &'a str,
    pub contract: &'a Contract,
    pub ledger: &'a Ledger,
    /// The wave about to be planned, from 1.
    pub wave: i64,
    pub max_turns: i64,
    pub max_chars: usize,
}

/// Guidance for a wave by its place in the budget: map the space first, then
/// go deep where coverage is thin.
pub fn phase_guidance(wave: i64, waves: i64) -> &'static str {
    if waves <= 1 {
        "This is the only wave: cover every cell at least once, favouring the questions \
         the deliverable cannot do without."
    } else if wave == 1 {
        "First wave: go broad. Map the space: touch every coverage cell, identify the main \
         entities and the best sources."
    } else if wave >= waves {
        "Last wave: go deep. Close gaps in the thinnest cells, resolve disputes, and \
         confirm single-source claims the deliverable depends on."
    } else {
        "Middle wave: follow the leads the findings opened, deepen thin cells, and \
         corroborate important single-source claims."
    }
}

/// Coverage lines, thinnest first: `(cell id, label, findings, corroborated)`.
pub fn coverage(contract: &Contract, ledger: &Ledger) -> Vec<(String, String, usize, usize)> {
    let mut rows: Vec<(String, String, usize, usize)> = contract
        .coverage
        .iter()
        .map(|c| {
            let found = ledger.findings_in_cell(&c.id);
            let strong = found
                .iter()
                .filter(|f| ledger.independent_sources(**f) >= 2)
                .count();
            (c.id.clone(), c.label.clone(), found.len(), strong)
        })
        .collect();
    rows.sort_by_key(|r| (r.3, r.2));
    rows
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Disputed => " [disputed]",
        Status::Pursued => " [being pursued]",
        _ => "",
    }
}

pub fn render(input: &ProjectionInput<'_>) -> String {
    let ProjectionInput {
        brief,
        contract,
        ledger,
        wave,
        max_turns,
        max_chars,
    } = *input;
    let waves = waves_for(max_turns);
    let mut lines: Vec<String> = Vec::new();

    lines.push("# The research so far".into());
    lines.push(String::new());
    lines.push(format!("**Brief.** {}", brief.trim()));
    lines.push(String::new());
    lines.push(contract.to_markdown());

    lines.push(format!("## Budget: wave {wave} of {waves}"));
    lines.push(phase_guidance(wave, waves).into());
    lines.push(String::new());

    lines.push(
        "## Coverage (thinnest first; corroborated = two or more independent sources)".into(),
    );
    for (id, label, n, strong) in coverage(contract, ledger) {
        lines.push(format!(
            "- `{id}` ({label}): {n} findings, {strong} corroborated"
        ));
    }
    lines.push(String::new());

    let mut open_q: Vec<(u8, i64, String)> = ledger
        .of_kind("question")
        .filter(|e| matches!(ledger.status(e.seq), Status::Open | Status::Pursued))
        .map(|e| {
            let p = match &e.body {
                Body::Question(q) => q.priority,
                _ => 2,
            };
            (
                p,
                e.seq,
                format!(
                    "- L{} [p{p}] {}{}",
                    e.seq,
                    e.body.headline(),
                    status_word(ledger.status(e.seq))
                ),
            )
        })
        .collect();
    open_q.sort_by_key(|(p, seq, _)| (*p, *seq));
    push_list(
        &mut lines,
        "## Open questions",
        open_q.into_iter().map(|t| t.2),
    );

    let mut open_leads: Vec<(u8, i64, String)> = ledger
        .of_kind("lead")
        .filter(|e| matches!(ledger.status(e.seq), Status::Open | Status::Pursued))
        .filter_map(|e| match &e.body {
            Body::Lead(l) => {
                let tag = if l.tangent { " (tangent)" } else { "" };
                let from: Vec<String> = ledger
                    .derived_from(e.seq)
                    .iter()
                    .map(|s| format!("L{s}"))
                    .collect();
                let from = if from.is_empty() {
                    String::new()
                } else {
                    format!(" — from {}", from.join(", "))
                };
                Some((
                    l.priority,
                    e.seq,
                    format!(
                        "- L{}{tag} [p{}] {}{from}{}",
                        e.seq,
                        l.priority,
                        e.body.headline(),
                        status_word(ledger.status(e.seq))
                    ),
                ))
            }
            _ => None,
        })
        .collect();
    open_leads.sort_by_key(|(p, seq, _)| (*p, *seq));
    push_list(
        &mut lines,
        "## Open leads",
        open_leads.into_iter().map(|t| t.2),
    );

    let paid: Vec<String> = ledger
        .paid_off_tangents()
        .into_iter()
        .map(|(lead, answers)| {
            let a: Vec<String> = answers.iter().map(|s| format!("L{s}")).collect();
            format!("- tangent L{lead} paid off: {}", a.join(", "))
        })
        .collect();
    push_list(&mut lines, "## Tangents that paid off", paid.into_iter());

    let disputes: Vec<String> = ledger
        .disputes()
        .into_iter()
        .map(|(a, b)| format!("- L{a} disputes L{b}"))
        .collect();
    push_list(&mut lines, "## Disputes", disputes.into_iter());

    let defects: Vec<String> = ledger
        .of_kind("defect")
        .filter(|e| ledger.status(e.seq) == Status::Open)
        .map(|e| format!("- L{} {}", e.seq, e.body.headline()))
        .collect();
    push_list(&mut lines, "## Unresolved defects", defects.into_iter());

    let dead: Vec<String> = ledger
        .of_kind("dead_end")
        .map(|e| format!("- {}", e.body.headline()))
        .collect();
    push_list(&mut lines, "## Dead ends (do not repeat)", dead.into_iter());

    let last = wave - 1;
    if last >= 1 {
        let fresh: Vec<String> = ledger
            .entries()
            .filter(|e| e.wave == last && e.body.kind() != "task")
            .map(|e| {
                let mut line = format!("- L{} {}: {}", e.seq, e.body.kind(), e.body.headline());
                if e.body.kind() == "finding" {
                    let cells: Vec<&str> = ledger.cells_of(e.seq).map(String::as_str).collect();
                    if cells.is_empty() {
                        line.push_str(" [unplaced]");
                    } else {
                        line.push_str(&format!(" [cells: {}]", cells.join(", ")));
                    }
                    if let Status::Duplicate(of) = ledger.status(e.seq) {
                        line.push_str(&format!(" [duplicate of L{of}]"));
                    }
                }
                line
            })
            .collect();
        let title = format!("## New in wave {last}");
        push_capped(&mut lines, &title, fresh.into_iter(), MAX_NEW);
    }

    fit(lines, max_chars)
}

fn push_list(lines: &mut Vec<String>, title: &str, items: impl Iterator<Item = String>) {
    push_capped(lines, title, items, MAX_LIST)
}

fn push_capped(
    lines: &mut Vec<String>,
    title: &str,
    items: impl Iterator<Item = String>,
    cap: usize,
) {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        return;
    }
    lines.push(title.to_string());
    let total = items.len();
    lines.extend(items.into_iter().take(cap));
    if total > cap {
        lines.push(format!(
            "- … {} more; read them with `research_view`.",
            total - cap
        ));
    }
    lines.push(String::new());
}

/// Join lines, stopping at a line boundary before `max_chars`.
fn fit(lines: Vec<String>, max_chars: usize) -> String {
    let mut out = String::new();
    let total = lines.len();
    for (i, line) in lines.into_iter().enumerate() {
        if out.len() + line.len() + 1 > max_chars {
            out.push_str(&format!(
                "\n… projection cut at {max_chars} characters ({} lines left); \
                 read the rest with `research_view`.\n",
                total - i
            ));
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::contract::tests::sample;
    use crate::research::ledger::tests::{finding, lead, link, row, source};
    use crate::storage::research::TagRow;

    fn ledger() -> Ledger {
        let rows = vec![
            row(
                1,
                1,
                &finding("Robbie appeared in 1940", "https://isfdb.org/t/1"),
            ),
            row(2, 1, &source("https://isfdb.org/t/1")),
            row(
                3,
                1,
                &finding("Robbie dates to 1940", "https://en.wikipedia.org/wiki/R"),
            ),
            row(4, 1, &lead("Look at radio adaptations", true)),
            row(5, 1, &lead("Read the 1950 collection", false)),
        ];
        let links = vec![
            link(1, 2, "cites"),
            link(3, 1, "duplicates"),
            link(4, 1, "derived_from"),
        ];
        let tags = vec![TagRow {
            seq: 1,
            tag_kind: "cell".into(),
            tag: "magazines".into(),
            wave: 1,
        }];
        Ledger::build(rows, links, tags)
    }

    #[test]
    fn a_projection_shows_budget_coverage_leads_and_the_last_wave() {
        let contract = sample();
        let l = ledger();
        let text = render(&ProjectionInput {
            brief: "SF and AI",
            contract: &contract,
            ledger: &l,
            wave: 2,
            max_turns: 4,
            max_chars: DEFAULT_MAX_CHARS,
        });
        assert!(text.contains("wave 2 of 2"), "{text}");
        assert!(text.contains("Last wave: go deep"), "{text}");
        assert!(
            text.contains("`magazines` (Short fiction in magazines): 1 findings, 1 corroborated"),
            "{text}"
        );
        assert!(text.contains("`novels` (Novels): 0 findings"), "{text}");
        assert!(
            text.contains("L4 (tangent) [p2] Look at radio adaptations"),
            "{text}"
        );
        assert!(text.contains("from L1"), "{text}");
        assert!(text.contains("## New in wave 1"), "{text}");
        assert!(text.contains("[duplicate of L1]"), "{text}");
        // Thinnest first (after the contract, which lists cells in its order).
        let meter = &text[text.find("## Coverage (thinnest").unwrap()..];
        assert!(meter.find("`novels`").unwrap() < meter.find("`magazines`").unwrap());
    }

    #[test]
    fn the_first_wave_has_no_new_entries_section() {
        let contract = sample();
        let l = Ledger::default();
        let text = render(&ProjectionInput {
            brief: "b",
            contract: &contract,
            ledger: &l,
            wave: 1,
            max_turns: 4,
            max_chars: DEFAULT_MAX_CHARS,
        });
        assert!(text.contains("First wave: go broad"));
        assert!(!text.contains("## New in wave"));
    }

    #[test]
    fn a_projection_respects_its_budget() {
        let contract = sample();
        let l = ledger();
        let text = render(&ProjectionInput {
            brief: "b",
            contract: &contract,
            ledger: &l,
            wave: 2,
            max_turns: 4,
            max_chars: 400,
        });
        assert!(text.len() < 600, "{}", text.len());
        assert!(text.contains("projection cut"));
    }
}
