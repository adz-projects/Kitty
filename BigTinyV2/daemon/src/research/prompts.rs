//! What each step of a research run is told, and the shapes its delegates
//! answer in. Kept apart from the driver so wording can be tuned without
//! reading control flow.

use serde_json::{json, Value};

use crate::research::contract::{waves_for, Contract};

/// The instructions for turn 0, sent alongside the user's brief.
pub fn plan_instructions(max_turns: i64, turns_given: bool) -> String {
    let budget = if turns_given {
        format!("The user set a budget of {max_turns} turns; put {max_turns} in `turns`.")
    } else {
        format!(
            "If the brief states a turn budget (\"use up to 10 turns\"), put that number in \
             `turns`; otherwise use {max_turns}."
        )
    };
    format!(
        "This is a deep research request. Do not answer it and do not research it yet. Your job \
         now is the research plan, which the user approves before any research starts.\n\n\
         Call `submit_research_plan` with:\n\
         - `title` and `objective`: what must be established, restated precisely.\n\
         - `deliverable`: what the finished output is (kind, audience, shape). `format` is \
         `markdown` unless the user asked for a Word document (`docx`). Set \
         `allow_unverified` only if the user would rather see a flagged unverified claim than \
         lose it.\n\
         - `sections`: the deliverable's outline, in order, each with a short lowercase `id`.\n\
         - `coverage`: the dimensions the research must cover, as cells the coverage meter can \
         count findings against (for example decade by medium, or party by record type). Short \
         lowercase ids.\n\
         - `questions`: the first questions to research.\n\
         - `acceptance`: how to tell the deliverable is good enough.\n\
         - `boundaries`: what is out of scope.\n\
         - `turns`: {budget} Each turn is one research wave, except the last two, which are \
         always a critique and the write-up.\n\n\
         If the brief is ambiguous in a way that changes the plan (which court, which period), \
         say what you assumed in `boundaries` rather than asking. If the user asks for \
         revisions, revise and call `submit_research_plan` again. Once it is approved, tell the \
         user in one sentence that the research has started, and stop."
    )
}

/// What a wave's planner is told, before the projection.
pub fn planner_prompt(projection: &str, wave: i64, max_turns: i64) -> String {
    let waves = waves_for(max_turns);
    let reading = if wave == 1 {
        "There are no findings yet. Plan the first frontier from the plan's questions and \
         coverage."
    } else {
        "1. Read what the last wave found (use `research_view` for entries in full).\n\
         2. Merge with `research_record`: link exact or near repeats (`duplicates`), \
         contradictions (`disputes`), newer replacing older (`supersedes`), findings that answer \
         a question or lead (`answers`); place unplaced findings in their coverage cells and \
         sections.\n\
         3. Write this wave's leads with `research_record`: next steps you read out of the \
         findings, each with `derived_from` naming the entries behind it. At least one must be a \
         productive tangent (`tangent: true`): something the findings suggest outside the plan's \
         coverage that could change or enrich the answer, such as a recurring name nobody asked \
         about, an unexpected related case, or a neighbouring genre. Note dead ends so they are \
         not repeated."
    };
    format!(
        "You are planning wave {wave} of {waves} of a deep research run. You do not research \
         yourself: you read the ledger, record what it means, and set the frontier that \
         delegates research in parallel when your turn ends.\n\n\
         {reading}\n\
         Then set the frontier with `research_set_frontier`: 3 or more tasks, each a full brief: \
         `objective` (what to find out), `output_format` (what to bring back), `guidance` \
         (which sources to try and what to avoid), `boundaries` (scope, dates, depth). Kind \
         `search` for the web, `read` for documents already found (put their paths or document \
         ids in `refs`), `compute` for figures. At least one task is a tangent. Once leads exist, \
         at least half the tasks pursue one (`pursues`). Favour the thinnest coverage cells. \
         Then end your turn with one sentence.\n\n{projection}"
    )
}

/// What one delegate is asked, from its frontier task.
pub fn task_prompt(
    objective: &str,
    output_format: &str,
    guidance: &str,
    boundaries: &str,
    brief: &str,
) -> String {
    format!(
        "Part of a larger research effort on: {brief}\n\n\
         Objective: {objective}\n\
         Bring back: {output_format}\n\
         Guidance: {guidance}\n\
         Boundaries: {boundaries}\n\n\
         Report only what your sources support. Each finding must be one self-contained claim \
         with the evidence quoted or closely paraphrased and the URL you read it at. If a claim \
         is someone's assertion rather than an established fact (a complaint's allegation, a \
         reviewer's opinion), say whose in the claim itself."
    )
}

pub const CRITIC_PROMPT: &str = "You are the adversarial reviewer of a research run. You did \
not do the research and you owe it nothing. Check the ledger and the draft outline against the \
plan and its acceptance criteria, and report defects: `unsupported` (a finding whose evidence \
does not carry its claim, or that rests on one weak source), `gap` (a coverage cell, section or \
acceptance criterion the evidence does not yet meet: give a `suggested_query` that would fill \
it), `contradiction` (findings that disagree and are not yet marked disputed), `scope` (material \
outside the plan's boundaries). Name the target as an entry (`L12`), a section id or a cell id. \
Be specific; a vague defect cannot be acted on. Also judge the tangents: list in \
`kept_tangents` the tangent findings that genuinely enrich the answer and belong in the \
deliverable, and leave out those that do not. You may open a source to check it.";

pub fn critic_schema() -> Value {
    strict(json!({
        "defects": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["unsupported", "gap", "contradiction", "scope"]},
                    "target": {"type": "string"},
                    "description": {"type": "string"},
                    "suggested_query": {"type": "string", "description": "Empty string if none."}
                },
                "required": ["kind", "target", "description", "suggested_query"],
                "additionalProperties": false
            }
        },
        "kept_tangents": {
            "type": "array",
            "description": "Entry ids (`L12`) of tangent findings worth keeping.",
            "items": {"type": "string"}
        }
    }))
}

pub const WRITER_PROMPT: &str = "You write one section of a research deliverable from the \
evidence you are given, and from nothing else. Every claim cites the finding it rests on, as \
[L12] (several as [L12, L40]), placed before the sentence's full stop. Do not cite anything not \
listed. Where findings dispute each other, present both and cite both. Attribute claims that \
are someone's assertion. If the evidence does not cover something the section should say, say \
plainly that it was not established rather than filling the gap. Write in Markdown without the \
section's own heading.";

pub fn writer_schema() -> Value {
    strict(json!({
        "markdown": {"type": "string", "description": "The section's text."}
    }))
}

pub const VERIFIER_PROMPT: &str = "You check a section of a research deliverable against the \
evidence its citations point to. For each sentence that makes a claim, decide whether the cited \
evidence supports it: `supported`, `partial` (the evidence supports a weaker claim), or \
`unsupported` (no citation, a citation to evidence that says something else, or more than the \
evidence carries). Quote each sentence exactly as written. Do not judge style.";

pub fn verifier_schema() -> Value {
    strict(json!({
        "verdicts": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "sentence": {"type": "string"},
                    "verdict": {"type": "string", "enum": ["supported", "partial", "unsupported"]},
                    "note": {"type": "string"}
                },
                "required": ["sentence", "verdict", "note"],
                "additionalProperties": false
            }
        }
    }))
}

pub const INTRO_PROMPT: &str = "You write the opening of a research deliverable: one or two \
short paragraphs saying what was asked, what the research established, and its main limits. \
Use only the section texts you are given; add no new facts and no citations.";

pub fn intro_schema() -> Value {
    strict(json!({
        "markdown": {"type": "string"}
    }))
}

/// The shape every specialist answer takes: its own fields plus `notes` and
/// `refusals`, strict-mode clean (see `specialists::registry::answer_schema`).
fn strict(props: Value) -> Value {
    let mut props = props.as_object().cloned().unwrap_or_default();
    props.insert(
        "notes".into(),
        json!({"type": "string", "description": "Anything else the caller should know. Empty string if none."}),
    );
    props.insert(
        "refusals".into(),
        json!({"type": "array", "description": "Tool calls that were refused. Empty array if none.", "items": {"type": "string"}}),
    );
    let required: Vec<String> = props.keys().cloned().collect();
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
        "additionalProperties": false,
    })
}

/// The plan's acceptance criteria and deliverable, for the critic and writers.
pub fn contract_brief(contract: &Contract) -> String {
    let mut out = format!(
        "Objective: {}\nDeliverable: {}\n",
        contract.objective, contract.deliverable.description
    );
    if !contract.acceptance.is_empty() {
        out.push_str("Acceptance criteria:\n");
        for a in &contract.acceptance {
            out.push_str(&format!("- {a}\n"));
        }
    }
    if !contract.boundaries.is_empty() {
        out.push_str("Boundaries:\n");
        for b in &contract.boundaries {
            out.push_str(&format!("- {b}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_strict_mode_clean() {
        for s in [
            critic_schema(),
            writer_schema(),
            verifier_schema(),
            intro_schema(),
        ] {
            let props = s["properties"].as_object().unwrap();
            let required: Vec<&str> = s["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(props.len(), required.len());
            assert_eq!(s["additionalProperties"], false);
            assert!(props.contains_key("notes") && props.contains_key("refusals"));
        }
    }

    #[test]
    fn the_planner_is_told_about_leads_only_once_there_are_findings() {
        assert!(!planner_prompt("P", 1, 4).contains("productive tangent (`tangent: true`)"));
        assert!(planner_prompt("P", 2, 4).contains("productive tangent (`tangent: true`)"));
        assert!(planner_prompt("P", 2, 4).contains("wave 2 of 2"));
    }

    #[test]
    fn a_given_budget_is_stated() {
        assert!(plan_instructions(6, true).contains("budget of 6 turns"));
        assert!(plan_instructions(4, false).contains("otherwise use 4"));
    }
}
