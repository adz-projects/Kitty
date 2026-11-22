//! The short updates a run posts in its chat at each milestone, so the user
//! can see the research moving without opening anything: what a wave did,
//! what it found, what the critique concluded, and what comes next. Built
//! from the ledger, not by a model: they cost nothing and cannot misreport.

use std::collections::BTreeSet;

use crate::research::contract::Contract;
use crate::research::ledger::{clip, Body, Ledger};

/// Most new findings named in a wave's note.
const NOTABLE: usize = 3;

/// A dead end the driver records for a delegate that never reported.
pub const DID_NOT_FINISH: &str = "The delegate did not finish";

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The note after wave `wave` of `waves`. `new` are the usable findings the
/// wave added, in ledger order.
pub fn wave_note(
    wave: i64,
    waves: i64,
    contract: &Contract,
    ledger: &Ledger,
    new: &[i64],
) -> String {
    let tasks: Vec<_> = ledger.of_kind("task").filter(|e| e.wave == wave).collect();
    let tangents = tasks
        .iter()
        .filter(|e| matches!(&e.body, Body::Task(t) if t.tangent))
        .count();
    let on_leads = tasks
        .iter()
        .filter(|e| {
            ledger
                .pursued_by(e.seq)
                .iter()
                .any(|p| matches!(ledger.get(*p).map(|x| &x.body), Some(Body::Lead(_))))
        })
        .count();
    let unfinished = ledger
        .of_kind("dead_end")
        .filter(|e| e.wave == wave)
        .filter(|e| matches!(&e.body, Body::DeadEnd(d) if d.note.starts_with(DID_NOT_FINISH)))
        .count();
    let sources: BTreeSet<i64> = new.iter().flat_map(|f| ledger.sources_of(*f)).collect();

    let mut out = format!(
        "**Deep research: wave {wave} of {waves} done.** {} ({}), {} from {}.",
        plural(tasks.len(), "task", "tasks"),
        if on_leads > 0 {
            format!(
                "{} following leads, {}",
                on_leads,
                plural(tangents, "tangent", "tangents")
            )
        } else {
            plural(tangents, "tangent", "tangents")
        },
        plural(new.len(), "new finding", "new findings"),
        plural(sources.len(), "source", "sources"),
    );
    if unfinished > 0 {
        out.push_str(&format!(
            " {} ran out of time.",
            plural(unfinished, "task", "tasks")
        ));
    }

    let reached = contract
        .coverage
        .iter()
        .filter(|c| !ledger.findings_in_cell(&c.id).is_empty())
        .count();
    out.push_str(&format!(
        "\n\nCoverage: {reached} of {} areas have findings.",
        contract.coverage.len()
    ));
    let thin: Vec<&str> = contract
        .coverage
        .iter()
        .filter(|c| ledger.findings_in_cell(&c.id).is_empty())
        .map(|c| c.label.as_str())
        .take(3)
        .collect();
    if !thin.is_empty() {
        out.push_str(&format!(" Still empty: {}.", thin.join("; ")));
    }

    let notable: Vec<String> = new
        .iter()
        .filter_map(|s| match ledger.get(*s).map(|e| &e.body) {
            Some(Body::Finding(f)) => Some(format!("- {}", clip(&f.claim, 160))),
            _ => None,
        })
        .take(NOTABLE)
        .collect();
    if !notable.is_empty() {
        out.push_str("\n\nAmong the new findings:\n");
        out.push_str(&notable.join("\n"));
    }

    out.push_str(&format!(
        "\n\nNext: {}.",
        if wave < waves {
            format!("wave {} of {waves}", wave + 1)
        } else {
            "the critique".to_string()
        }
    ));
    out
}

/// The note after the critique.
pub fn critique_note(defects: usize, gaps: usize, gap_findings: usize, kept: usize) -> String {
    let mut out = format!(
        "**Deep research: critique done.** The reviewer raised {}.",
        plural(defects, "issue", "issues")
    );
    if gaps > 0 {
        out.push_str(&format!(
            " {} searched again, adding {}.",
            plural(gaps, "gap was", "gaps were"),
            plural(gap_findings, "finding", "findings")
        ));
    }
    if kept > 0 {
        out.push_str(&format!(
            " {} kept for the report.",
            plural(kept, "tangent", "tangents")
        ));
    }
    out.push_str("\n\nNext: the write-up.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::contract::tests::sample;
    use crate::research::ledger::tests::{finding, lead, link, row, source};
    use crate::research::ledger::{DeadEnd, Task, TaskKind};
    use crate::storage::research::TagRow;

    fn task(tangent: bool) -> Body {
        Body::Task(Task {
            kind: TaskKind::Search,
            objective: "x".into(),
            output_format: "y".into(),
            guidance: "z".into(),
            boundaries: "w".into(),
            refs: vec![],
            tangent,
        })
    }

    #[test]
    fn a_wave_note_says_what_happened_and_what_is_next() {
        let rows = vec![
            row(1, 1, &lead("Read I, Robot", false)),
            row(2, 2, &task(false)),
            row(3, 2, &task(true)),
            row(4, 2, &source("https://isfdb.org/1")),
            row(
                5,
                2,
                &finding("Robbie appeared in 1940", "https://isfdb.org/1"),
            ),
            row(
                6,
                2,
                &Body::DeadEnd(DeadEnd {
                    query: "x".into(),
                    note: format!("{DID_NOT_FINISH}: ran longer than 300s"),
                }),
            ),
        ];
        let links = vec![link(2, 1, "pursues"), link(5, 4, "cites")];
        let tags = vec![TagRow {
            seq: 5,
            tag_kind: "cell".into(),
            tag: "magazines".into(),
            wave: 2,
        }];
        let l = Ledger::build(rows, links, tags);
        let note = wave_note(2, 3, &sample(), &l, &[5]);
        assert!(note.starts_with("**Deep research: wave 2 of 3 done.** 2 tasks (1 following leads, 1 tangent), 1 new finding from 1 source."), "{note}");
        assert!(note.contains("1 task ran out of time."), "{note}");
        assert!(
            note.contains("Coverage: 1 of 2 areas have findings. Still empty: Novels."),
            "{note}"
        );
        assert!(note.contains("- Robbie appeared in 1940"), "{note}");
        assert!(note.ends_with("Next: wave 3 of 3."), "{note}");
        assert!(wave_note(3, 3, &sample(), &l, &[]).ends_with("Next: the critique."));
    }

    #[test]
    fn a_critique_note_counts_what_it_did() {
        assert_eq!(
            critique_note(4, 2, 5, 1),
            "**Deep research: critique done.** The reviewer raised 4 issues. 2 gaps were \
             searched again, adding 5 findings. 1 tangent kept for the report.\n\nNext: the \
             write-up."
        );
        assert!(critique_note(0, 0, 0, 0).contains("raised 0 issues."));
    }
}
