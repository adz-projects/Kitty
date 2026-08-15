//! Internal, wire-agnostic diagram/chart/table data model plus validation.
//! Deliberately mirrors rather than reuses `server.rs`'s `#[derive(JsonSchema)]`
//! wire structs: it keeps this file (and the layout modules downstream of it)
//! testable without MCP wire types in scope, and lets the wire schema evolve
//! for prompt-engineering reasons — new field descriptions, enum framing —
//! without touching validation or layout logic.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::envelope::error_response;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramType {
    SingleLane,
    Flowchart,
    Tree,
    Swimlane,
    JourneyMap,
}

pub fn diagram_type_name(t: DiagramType) -> &'static str {
    match t {
        DiagramType::SingleLane => "single_lane",
        DiagramType::Flowchart => "flowchart",
        DiagramType::Tree => "tree",
        DiagramType::Swimlane => "swimlane",
        DiagramType::JourneyMap => "journey_map",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepType {
    Start,
    #[default]
    Process,
    Decision,
    End,
}

#[derive(Debug, Clone, Default)]
pub struct Step {
    pub id: Option<String>,
    pub text: String,
    pub step_type: StepType,
    pub subtitle: Option<String>,
    pub lane: Option<String>,
    pub sentiment: Option<i32>,
    pub pain: Option<String>,
    pub next: Vec<String>,
}

/// General node cap. Swimlane and journey_map get tighter caps — see the
/// layout modules for why (a monotonic time axis can't wrap; fixed stage
/// columns don't compose past a handful).
pub const MAX_NODES_GENERAL: usize = 40;
pub const MAX_NODES_SWIMLANE: usize = 24;
pub const MAX_NODES_JOURNEY: usize = 12;
pub const MAX_CHART_CATEGORIES: usize = 24;

#[derive(Debug)]
pub struct ValidatedDiagram {
    pub diagram_type: DiagramType,
    pub title: String,
    pub description: String,
    pub steps: Vec<Step>,
    pub warnings: Vec<String>,
}

/// Validates and normalizes a diagram request. `Err` carries a complete,
/// pretty-printed `envelope::error_response` JSON payload, ready to return
/// directly as the tool's output. Fields that don't apply to the given
/// `diagram_type` are warnings, not errors — the diagram is still renderable
/// with them simply ignored, so rejecting the call outright would be hostile.
pub fn validate_diagram(diagram_type: DiagramType, title: &str, description: &str, steps: Vec<Step>) -> Result<ValidatedDiagram, String> {
    let mut warnings = Vec::new();

    let non_empty: Vec<Step> = steps.into_iter().filter(|s| !s.text.trim().is_empty()).collect();
    if non_empty.is_empty() {
        return Err(error_response(
            "VIZ_EMPTY_STEPS",
            "No usable steps were provided.",
            None,
            Some("Provide at least one step with non-empty `text`."),
        ));
    }

    let cap = match diagram_type {
        DiagramType::Swimlane => MAX_NODES_SWIMLANE,
        DiagramType::JourneyMap => MAX_NODES_JOURNEY,
        _ => MAX_NODES_GENERAL,
    };
    if non_empty.len() > cap {
        return Err(error_response(
            "VIZ_TOO_MANY_NODES",
            &format!("{} steps were provided, but \"{}\" supports at most {cap}.", non_empty.len(), diagram_type_name(diagram_type)),
            None,
            Some(&format!("Split into multiple diagrams, or summarize down to at most {cap} steps.")),
        ));
    }

    if matches!(diagram_type, DiagramType::Flowchart | DiagramType::Tree) {
        let known_ids: HashSet<&str> = non_empty.iter().filter_map(|s| s.id.as_deref()).collect();
        for step in &non_empty {
            for next_id in &step.next {
                if !known_ids.contains(next_id.as_str()) {
                    let mut ids: Vec<&str> = known_ids.iter().copied().collect();
                    ids.sort();
                    return Err(error_response(
                        "VIZ_BAD_EDGE_REF",
                        &format!("`next` references id \"{next_id}\", which does not match any step's `id`."),
                        None,
                        Some(&format!("Every value in `next` must match another step's `id`. Known ids: [{}]", ids.join(", "))),
                    ));
                }
            }
        }

        // Long edges (an edge spanning two or more layers) would route their
        // connector straight through the intermediate row's nodes — there is no
        // collision-avoidance pass in the layout engine, so the foolproof
        // answer is to reject them and ask for intermediate steps.
        reject_skip_level_edges(&non_empty)?;

        if diagram_type == DiagramType::Tree {
            let mut parent_count: HashMap<&str, usize> = HashMap::new();
            for step in &non_empty {
                for next_id in &step.next {
                    *parent_count.entry(next_id.as_str()).or_insert(0) += 1;
                }
            }
            if let Some((&dup_id, _)) = parent_count.iter().find(|(_, &count)| count > 1) {
                return Err(error_response(
                    "VIZ_BAD_EDGE_REF",
                    &format!("Node \"{dup_id}\" is listed under `next` by more than one step; a tree node can have only one parent."),
                    None,
                    Some("Use \"flowchart\" instead of \"tree\" if a node can be reached more than one way."),
                ));
            }
        }
    }

    if diagram_type == DiagramType::Swimlane {
        let any_lane = non_empty.iter().any(|s| s.lane.as_deref().is_some_and(|l| !l.trim().is_empty()));
        if !any_lane {
            return Err(error_response(
                "VIZ_MISSING_LANES",
                "No step specified a `lane`.",
                None,
                Some("Give each step a `lane` naming who performs it, e.g. \"Customer\"."),
            ));
        }
    }

    for (idx, step) in non_empty.iter().enumerate() {
        let mut irrelevant: Vec<&str> = Vec::new();
        match diagram_type {
            DiagramType::SingleLane => {
                if step.lane.is_some() {
                    irrelevant.push("lane");
                }
                if step.sentiment.is_some() {
                    irrelevant.push("sentiment");
                }
                if step.pain.is_some() {
                    irrelevant.push("pain");
                }
                if !step.next.is_empty() {
                    irrelevant.push("next");
                }
            }
            DiagramType::JourneyMap => {
                if step.lane.is_some() {
                    irrelevant.push("lane");
                }
                if !step.next.is_empty() {
                    irrelevant.push("next");
                }
            }
            DiagramType::Swimlane => {
                if step.sentiment.is_some() {
                    irrelevant.push("sentiment");
                }
                if step.pain.is_some() {
                    irrelevant.push("pain");
                }
            }
            DiagramType::Tree | DiagramType::Flowchart => {
                if step.lane.is_some() {
                    irrelevant.push("lane");
                }
                if step.sentiment.is_some() {
                    irrelevant.push("sentiment");
                }
                if step.pain.is_some() {
                    irrelevant.push("pain");
                }
            }
        }
        if !irrelevant.is_empty() {
            warnings.push(format!("Step {idx}: {} ignored by \"{}\".", join_backtick(&irrelevant), diagram_type_name(diagram_type)));
        }
    }

    Ok(ValidatedDiagram { diagram_type, title: title.to_string(), description: description.to_string(), steps: non_empty, warnings })
}

fn join_backtick(fields: &[&str]) -> String {
    fields.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join("/")
}

/// Computes each node's longest-path layer (Bellman-Ford-style relaxation)
/// and rejects any edge whose target is more than one layer below its
/// source. Mirrors `compute_layers` in `layout::graph` for the same reason
/// `compute_layers`'s Kahn loop exists: roots with no incoming edge start at
/// layer 0.
///
/// The relaxation is **bounded to `steps.len()` passes**: a DAG's longest
/// path has at most N-1 edges, so a DAG always converges within N passes.
/// A pass that still changes values after that bound proves a cycle exists
/// (each pass keeps pushing the cycle's members deeper forever — the old
/// unbounded loop hung the tool on `a → b → a`), and the cycle is rejected
/// rather than looped on.
fn reject_skip_level_edges(steps: &[Step]) -> Result<(), String> {
    let mut depth: HashMap<&str, usize> = HashMap::new();
    let mut converged = false;
    for _pass in 0..steps.len() {
        let mut changed = false;
        for step in steps.iter().filter(|s| s.id.is_some()) {
            let id = step.id.as_deref().unwrap();
            let d = *depth.get(id).unwrap_or(&0);
            for nxt in &step.next {
                let nd = depth.entry(nxt.as_str()).or_insert(0);
                if d + 1 > *nd {
                    *nd = d + 1;
                    changed = true;
                }
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }
    if !converged {
        return Err(error_response(
            "VIZ_BAD_EDGE_REF",
            "The `next` edges contain a cycle, so no layer assignment exists.",
            None,
            Some("Remove the edge that loops back to an earlier step; a flowchart's `next` edges must form a DAG."),
        ));
    }

    for step in steps.iter().filter(|s| s.id.is_some()) {
        let id = step.id.as_deref().unwrap();
        let ds = *depth.get(id).unwrap_or(&0);
        for nxt in &step.next {
            let dt = *depth.get(nxt.as_str()).unwrap_or(&0);
            if dt > ds + 1 {
                return Err(error_response(
                    "VIZ_LONG_EDGE",
                    &format!("Edge \"{id}\" → \"{nxt}\" jumps {jump} layer(s), which would route the connector straight through the intermediate row.", jump = dt - ds),
                    None,
                    Some("Insert intermediate steps so every `next` edge links only to the immediately following layer."),
                ));
            }
        }
    }
    Ok(())
}

// ---- table validation ----

pub fn validate_table(headers: &[String], rows: &[Vec<Value>]) -> Result<(), String> {
    if headers.is_empty() {
        return Err(error_response("VIZ_EMPTY_TABLE", "No headers were provided.", None, Some("Provide at least one column header.")));
    }
    if rows.is_empty() {
        return Err(error_response("VIZ_EMPTY_TABLE", "No rows were provided.", None, Some("Provide at least one row of data.")));
    }
    for (idx, row) in rows.iter().enumerate() {
        if row.len() != headers.len() {
            return Err(error_response(
                "VIZ_RAGGED_ROWS",
                &format!("Row {idx} has {} value(s) but there are {} headers.", row.len(), headers.len()),
                None,
                Some("Every row must have exactly as many values as there are headers."),
            ));
        }
    }
    Ok(())
}

// ---- chart validation ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartType {
    Bar,
    HorizontalBar,
    Line,
    GroupedBar,
}

#[derive(Debug, Clone)]
pub struct ChartSeries {
    pub name: String,
    pub values: Vec<f64>,
}

pub fn validate_chart(categories: &[String], series: &[ChartSeries]) -> Result<(), String> {
    if categories.is_empty() {
        return Err(error_response("VIZ_EMPTY_TABLE", "No categories were provided.", None, Some("Provide at least one category label.")));
    }
    if categories.len() > MAX_CHART_CATEGORIES {
        return Err(error_response(
            "VIZ_TOO_MANY_NODES",
            &format!("{} categories were provided, but charts support at most {MAX_CHART_CATEGORIES}.", categories.len()),
            None,
            Some(&format!("Summarize down to at most {MAX_CHART_CATEGORIES} categories.")),
        ));
    }
    if series.is_empty() {
        return Err(error_response("VIZ_EMPTY_TABLE", "No series were provided.", None, Some("Provide at least one series of values.")));
    }
    for s in series {
        if s.values.len() != categories.len() {
            return Err(error_response(
                "VIZ_SERIES_LENGTH_MISMATCH",
                &format!("Series \"{}\" has {} value(s) but there are {} categories.", s.name, s.values.len(), categories.len()),
                None,
                Some("Every series must have exactly one value per category."),
            ));
        }
        if s.values.iter().any(|v| !v.is_finite()) {
            return Err(error_response(
                "VIZ_BAD_NUMBER",
                &format!("Series \"{}\" contains a non-finite value.", s.name),
                None,
                Some("Use finite numbers."),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(text: &str) -> Step {
        Step { text: text.to_string(), ..Default::default() }
    }

    #[test]
    fn rejects_all_blank_steps() {
        let err = validate_diagram(DiagramType::SingleLane, "T", "D", vec![step(""), step("   ")]).unwrap_err();
        assert!(err.contains("VIZ_EMPTY_STEPS"));
        assert!(err.contains("\"hint\""));
    }

    #[test]
    fn filters_blank_steps_but_keeps_the_rest() {
        let v = validate_diagram(DiagramType::SingleLane, "T", "D", vec![step(""), step("Real step")]).unwrap();
        assert_eq!(v.steps.len(), 1);
        assert_eq!(v.steps[0].text, "Real step");
    }

    #[test]
    fn rejects_too_many_nodes_for_swimlane() {
        let steps: Vec<Step> = (0..30).map(|i| Step { text: format!("s{i}"), lane: Some("A".into()), ..Default::default() }).collect();
        let err = validate_diagram(DiagramType::Swimlane, "T", "D", steps).unwrap_err();
        assert!(err.contains("VIZ_TOO_MANY_NODES"));
    }

    #[test]
    fn rejects_unknown_next_reference() {
        let steps = vec![
            Step { id: Some("a".into()), text: "A".into(), next: vec!["ghost".into()], ..Default::default() },
        ];
        let err = validate_diagram(DiagramType::Flowchart, "T", "D", steps).unwrap_err();
        assert!(err.contains("VIZ_BAD_EDGE_REF"));
        assert!(err.contains("\"hint\""));
    }

    #[test]
    fn rejects_a_cycle_instead_of_hanging() {
        // Audit #110: a 2-node cycle (a → b → a) sent the layer-relaxation
        // loop into an infinite loop — each pass kept pushing the cycle's
        // depths higher. The loop is now bounded and the cycle rejected
        // through the validation entry point (not just at render).
        let steps = vec![
            Step { id: Some("a".into()), text: "A".into(), next: vec!["b".into()], ..Default::default() },
            Step { id: Some("b".into()), text: "B".into(), next: vec!["a".into()], ..Default::default() },
        ];
        let err = validate_diagram(DiagramType::Flowchart, "T", "D", steps).unwrap_err();
        assert!(err.contains("VIZ_BAD_EDGE_REF"), "got: {err}");
        assert!(err.contains("cycle"), "the message should name the problem: {err}");
        assert!(err.contains("\"hint\""));

        // A self-loop is the one-node form of the same bug.
        let steps = vec![Step { id: Some("a".into()), text: "A".into(), next: vec!["a".into()], ..Default::default() }];
        let err = validate_diagram(DiagramType::Flowchart, "T", "D", steps).unwrap_err();
        assert!(err.contains("VIZ_BAD_EDGE_REF"), "got: {err}");
    }

    #[test]
    fn rejects_tree_node_with_two_parents() {
        let steps = vec![
            Step { id: Some("a".into()), text: "A".into(), next: vec!["c".into()], ..Default::default() },
            Step { id: Some("b".into()), text: "B".into(), next: vec!["c".into()], ..Default::default() },
            Step { id: Some("c".into()), text: "C".into(), ..Default::default() },
        ];
        let err = validate_diagram(DiagramType::Tree, "T", "D", steps).unwrap_err();
        assert!(err.contains("VIZ_BAD_EDGE_REF"));
    }

    #[test]
    fn allows_dag_merge_in_flowchart() {
        let steps = vec![
            Step { id: Some("a".into()), text: "A".into(), next: vec!["c".into()], ..Default::default() },
            Step { id: Some("b".into()), text: "B".into(), next: vec!["c".into()], ..Default::default() },
            Step { id: Some("c".into()), text: "C".into(), ..Default::default() },
        ];
        let v = validate_diagram(DiagramType::Flowchart, "T", "D", steps).unwrap();
        assert_eq!(v.steps.len(), 3);
    }

    #[test]
    fn rejects_swimlane_with_no_lanes() {
        let err = validate_diagram(DiagramType::Swimlane, "T", "D", vec![step("A"), step("B")]).unwrap_err();
        assert!(err.contains("VIZ_MISSING_LANES"));
    }

    #[test]
    fn warns_on_irrelevant_fields_but_still_renders() {
        let steps = vec![Step { text: "A".into(), sentiment: Some(1), ..Default::default() }];
        let v = validate_diagram(DiagramType::SingleLane, "T", "D", steps).unwrap();
        assert_eq!(v.steps.len(), 1);
        assert!(!v.warnings.is_empty());
    }

    #[test]
    fn every_diagram_error_has_a_hint() {
        // envelope::auto_hint only recognizes NOT_FOUND/MISSING/CORRUPT/PARSE/
        // BAD_RANGE/SEARCH -- none of which any VIZ_* code contains, so every
        // viz error must pass its hint explicitly or the model sees nothing.
        let cases: Vec<Result<ValidatedDiagram, String>> = vec![
            validate_diagram(DiagramType::SingleLane, "T", "D", vec![]),
            validate_diagram(DiagramType::Swimlane, "T", "D", vec![step("A")]),
        ];
        for case in cases {
            let err = case.unwrap_err();
            assert!(err.contains("\"hint\""), "missing hint in: {err}");
        }
    }

    #[test]
    fn table_rejects_ragged_rows() {
        let err = validate_table(&["A".to_string(), "B".to_string()], &[vec![json!(1)]]).unwrap_err();
        assert!(err.contains("VIZ_RAGGED_ROWS"));
        assert!(err.contains("\"hint\""));
    }

    #[test]
    fn table_rejects_empty_headers_or_rows() {
        assert!(validate_table(&[], &[vec![json!(1)]]).is_err());
        assert!(validate_table(&["A".to_string()], &[]).is_err());
    }

    #[test]
    fn chart_rejects_series_length_mismatch() {
        let series = vec![ChartSeries { name: "S".to_string(), values: vec![1.0, 2.0] }];
        let err = validate_chart(&["Q1".to_string(), "Q2".to_string(), "Q3".to_string()], &series).unwrap_err();
        assert!(err.contains("VIZ_SERIES_LENGTH_MISMATCH"));
    }

    #[test]
    fn chart_rejects_non_finite_values() {
        let series = vec![ChartSeries { name: "S".to_string(), values: vec![1.0, f64::NAN] }];
        let err = validate_chart(&["Q1".to_string(), "Q2".to_string()], &series).unwrap_err();
        assert!(err.contains("VIZ_BAD_NUMBER"));
    }

    #[test]
    fn chart_rejects_too_many_categories() {
        let categories: Vec<String> = (0..30).map(|i| format!("c{i}")).collect();
        let series = vec![ChartSeries { name: "S".to_string(), values: vec![1.0; 30] }];
        let err = validate_chart(&categories, &series).unwrap_err();
        assert!(err.contains("VIZ_TOO_MANY_NODES"));
    }
}
