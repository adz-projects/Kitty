//! The research contract: what turn 0 proposes and the user approves.
//!
//! Every later step is measured against it. Its coverage cells drive the
//! coverage meter and the wave planner's priorities; its sections are the
//! writer's outline; its acceptance criteria are what the critic checks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Turns when the user names none: two waves, the critic, the write-up.
pub const DEFAULT_TURNS: i64 = 4;
/// One wave plus the two reserved turns.
pub const MIN_TURNS: i64 = 3;
pub const MAX_TURNS: i64 = 20;
/// Turns at the end that are never spent on research: critic, write-up.
pub const RESERVED_TURNS: i64 = 2;

const MAX_SECTIONS: usize = 20;
const MAX_CELLS: usize = 40;
const MAX_QUESTIONS: usize = 30;

/// `n` clamped to the allowed budget, or the default when absent.
pub fn clamp_turns(n: Option<i64>) -> i64 {
    n.unwrap_or(DEFAULT_TURNS).clamp(MIN_TURNS, MAX_TURNS)
}

/// Research waves a budget of `max_turns` allows.
pub fn waves_for(max_turns: i64) -> i64 {
    (max_turns - RESERVED_TURNS).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// A Markdown file (always written) and the chat message.
    #[default]
    Markdown,
    /// Markdown plus a Word document.
    Docx,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Deliverable {
    /// What the finished output is, in a sentence or two: its kind, audience
    /// and shape ("an annotated bibliography, one entry per work, …").
    pub description: String,
    /// `markdown` unless the user asked for a Word document.
    #[serde(default)]
    pub format: OutputFormat,
    /// Whether a claim that could not be verified may stay in the output,
    /// marked "(unverified)", instead of being cut.
    #[serde(default)]
    pub allow_unverified: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Section {
    /// Short lowercase id, e.g. `parties` or `1950s`.
    pub id: String,
    pub title: String,
    /// What belongs in this section.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Cell {
    /// Short lowercase id, e.g. `1940s-magazines` or `plaintiff-court-records`.
    pub id: String,
    /// What evidence counts toward this part of the space.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Contract {
    pub title: String,
    /// What the research must establish, restated precisely.
    pub objective: String,
    pub deliverable: Deliverable,
    /// The outline of the deliverable, in order.
    pub sections: Vec<Section>,
    /// The dimensions the research must cover. Each is a cell the coverage
    /// meter counts findings against: e.g. decade by medium, or party by
    /// record type.
    pub coverage: Vec<Cell>,
    /// The first questions to research.
    pub questions: Vec<String>,
    /// How to tell the deliverable is good enough.
    #[serde(default)]
    pub acceptance: Vec<String>,
    /// What is out of scope, and limits the research must respect.
    #[serde(default)]
    pub boundaries: Vec<String>,
    /// Turn budget, counting the waves, the critique and the write-up.
    /// Use the number the user gave, else 4.
    #[serde(default = "default_turns")]
    pub turns: i64,
}

fn default_turns() -> i64 {
    DEFAULT_TURNS
}

/// `raw` as a contract id: lowercase letters, digits, `-` and `_`, at most 48
/// characters; `fallback` when nothing usable is left.
fn slug(raw: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(48).collect();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

/// Make every id a valid, unique slug in place.
fn repair_ids<'a>(ids: impl Iterator<Item = &'a mut String>, prefix: &str) {
    let mut seen = std::collections::HashSet::new();
    for (i, id) in ids.enumerate() {
        let base = slug(id, &format!("{prefix}{}", i + 1));
        let mut candidate = base.clone();
        let mut n = 2;
        while !seen.insert(candidate.clone()) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        *id = candidate;
    }
}

impl Contract {
    /// Repair what can be repaired in a contract a model proposed (ids made
    /// valid and unique, lists cut to their limits, the budget clamped) and
    /// refuse only what cannot: an empty title, objective or deliverable, or
    /// no sections, coverage or questions at all. Repairing rather than
    /// refusing matters because the user has usually approved the plan by the
    /// time it is checked. The error is written for the model.
    pub fn validate(&mut self) -> Result<(), String> {
        let mut problems = Vec::new();
        if self.title.trim().is_empty() {
            problems.push("`title` is empty");
        }
        if self.objective.trim().is_empty() {
            problems.push("`objective` is empty");
        }
        if self.deliverable.description.trim().is_empty() {
            problems.push("`deliverable.description` is empty");
        }
        self.sections.retain(|s| !s.title.trim().is_empty());
        self.sections.truncate(MAX_SECTIONS);
        self.coverage
            .retain(|c| !c.label.trim().is_empty() || !c.id.trim().is_empty());
        self.coverage.truncate(MAX_CELLS);
        self.questions.retain(|q| !q.trim().is_empty());
        self.questions.truncate(MAX_QUESTIONS);
        if self.sections.is_empty() {
            problems.push("give at least one section");
        }
        if self.coverage.is_empty() {
            problems.push("give at least one coverage cell");
        }
        if self.questions.is_empty() {
            problems.push("give at least one question");
        }
        repair_ids(self.sections.iter_mut().map(|s| &mut s.id), "section-");
        repair_ids(self.coverage.iter_mut().map(|c| &mut c.id), "cell-");
        self.turns = clamp_turns(Some(self.turns));
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "The plan was not accepted: {}.",
                problems.join("; ")
            ))
        }
    }

    pub fn has_section(&self, id: &str) -> bool {
        self.sections.iter().any(|s| s.id == id)
    }

    pub fn has_cell(&self, id: &str) -> bool {
        self.coverage.iter().any(|c| c.id == id)
    }

    /// The contract as Markdown, for the plan card and for prompts.
    pub fn to_markdown(&self) -> String {
        let mut out = format!("# {}\n\n**Objective.** {}\n\n", self.title, self.objective);
        let format = match self.deliverable.format {
            OutputFormat::Markdown => "Markdown",
            OutputFormat::Docx => "Markdown and Word",
        };
        out.push_str(&format!(
            "**Deliverable** ({format}). {}\n\n",
            self.deliverable.description
        ));
        out.push_str(&format!(
            "**Budget.** {} turns: {} research wave{}, a critique, the write-up.\n\n",
            self.turns,
            waves_for(self.turns),
            if waves_for(self.turns) == 1 { "" } else { "s" }
        ));
        out.push_str("## Outline\n");
        for s in &self.sections {
            out.push_str(&format!(
                "- **{}** (`{}`): {}\n",
                s.title, s.id, s.description
            ));
        }
        out.push_str("\n## Coverage\n");
        for c in &self.coverage {
            out.push_str(&format!("- `{}`: {}\n", c.id, c.label));
        }
        out.push_str("\n## First questions\n");
        for q in &self.questions {
            out.push_str(&format!("- {q}\n"));
        }
        if !self.acceptance.is_empty() {
            out.push_str("\n## Acceptance criteria\n");
            for a in &self.acceptance {
                out.push_str(&format!("- {a}\n"));
            }
        }
        if !self.boundaries.is_empty() {
            out.push_str("\n## Boundaries\n");
            for b in &self.boundaries {
                out.push_str(&format!("- {b}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> Contract {
        Contract {
            title: "AI in early SF".into(),
            objective: "Annotated bibliography of 1940-1970 SF about AI".into(),
            deliverable: Deliverable {
                description: "One entry per work".into(),
                format: OutputFormat::Markdown,
                allow_unverified: false,
            },
            sections: vec![
                Section {
                    id: "1940s".into(),
                    title: "The 1940s".into(),
                    description: "Works first published 1940-49".into(),
                },
                Section {
                    id: "1950s".into(),
                    title: "The 1950s".into(),
                    description: "Works first published 1950-59".into(),
                },
            ],
            coverage: vec![
                Cell {
                    id: "magazines".into(),
                    label: "Short fiction in magazines".into(),
                },
                Cell {
                    id: "novels".into(),
                    label: "Novels".into(),
                },
            ],
            questions: vec!["Which Astounding stories feature thinking machines?".into()],
            acceptance: vec!["Every entry cites a bibliographic source".into()],
            boundaries: vec!["No film".into()],
            turns: 4,
        }
    }

    #[test]
    fn budgets_default_to_four_and_clamp() {
        assert_eq!(clamp_turns(None), 4);
        assert_eq!(clamp_turns(Some(1)), MIN_TURNS);
        assert_eq!(clamp_turns(Some(99)), MAX_TURNS);
        assert_eq!(waves_for(4), 2);
        assert_eq!(waves_for(3), 1);
    }

    #[test]
    fn a_good_contract_validates_and_a_bad_one_says_why() {
        let mut good = sample();
        assert!(good.validate().is_ok());

        let mut fixable = sample();
        fixable.sections[1].id = "1940s".into();
        fixable.coverage[0].id = "Bad Id!".into();
        fixable.coverage[1].id = "".into();
        fixable.turns = 50;
        assert!(fixable.validate().is_ok());
        assert_eq!(fixable.sections[1].id, "1940s-2");
        assert_eq!(fixable.coverage[0].id, "bad-id");
        assert_eq!(fixable.coverage[1].id, "cell-2");
        assert_eq!(fixable.turns, MAX_TURNS);

        let mut bad = sample();
        bad.questions = vec!["  ".into()];
        bad.objective = String::new();
        let err = bad.validate().unwrap_err();
        assert!(err.contains("objective"), "{err}");
        assert!(err.contains("question"), "{err}");
    }

    #[test]
    fn a_missing_budget_reads_as_four() {
        let mut v = serde_json::to_value(sample()).unwrap();
        v.as_object_mut().unwrap().remove("turns");
        let c: Contract = serde_json::from_value(v).unwrap();
        assert_eq!(c.turns, 4);
    }

    #[test]
    fn markdown_names_every_part() {
        let md = sample().to_markdown();
        for needle in [
            "# AI in early SF",
            "2 research waves",
            "`magazines`",
            "No film",
        ] {
            assert!(md.contains(needle), "missing {needle}: {md}");
        }
    }
}
