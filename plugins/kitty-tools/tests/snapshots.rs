//! Golden-file snapshots, one per layout, guarding against unintentional
//! output drift. Rendering is fully deterministic (no randomness, no
//! timestamps), so a byte-for-byte diff is meaningful. Run with
//! `KITTY_UPDATE_SNAPSHOTS=1 cargo test --test snapshots` to regenerate after
//! an intentional visual change.

use std::fs;
use std::path::PathBuf;

use kitty_tools::tools::viz::model::{ChartSeries, ChartType, DiagramType, Step, StepType};
use kitty_tools::tools::viz::{generate_accessible_chart, generate_accessible_svg, generate_accessible_table};

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

fn assert_snapshot(name: &str, actual: &str) {
    let path = snapshot_dir().join(format!("{name}.json"));
    if std::env::var("KITTY_UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        fs::create_dir_all(snapshot_dir()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|_| panic!("no snapshot at {path:?} -- run with KITTY_UPDATE_SNAPSHOTS=1 to create it"));
    assert_eq!(actual, expected, "snapshot \"{name}\" changed -- if intentional, rerun with KITTY_UPDATE_SNAPSHOTS=1 to update it");
}

fn step(text: &str) -> Step {
    Step { text: text.to_string(), ..Default::default() }
}

#[test]
fn single_lane_snapshot() {
    let steps = vec![step("Ingest data"), step("Validate schema"), step("Publish event")];
    let out = generate_accessible_svg(DiagramType::SingleLane, "Ingestion pipeline", "A three-step data ingestion pipeline.", steps);
    assert_snapshot("single_lane", &out);
}

#[test]
fn flowchart_snapshot() {
    let steps = vec![
        Step { id: Some("a".into()), text: "Receive order".into(), step_type: StepType::Start, next: vec!["b".into()], ..Default::default() },
        Step {
            id: Some("b".into()),
            text: "Payment ok?".into(),
            step_type: StepType::Decision,
            next: vec!["c".into(), "d".into()],
            ..Default::default()
        },
        Step { id: Some("c".into()), text: "Ship order".into(), step_type: StepType::End, ..Default::default() },
        Step { id: Some("d".into()), text: "Cancel order".into(), step_type: StepType::End, ..Default::default() },
    ];
    let out = generate_accessible_svg(DiagramType::Flowchart, "Order fulfillment", "How an order is fulfilled or cancelled.", steps);
    assert_snapshot("flowchart", &out);
}

#[test]
fn tree_snapshot() {
    let steps = vec![
        Step { id: Some("ceo".into()), text: "CEO".into(), next: vec!["eng".into(), "sales".into()], ..Default::default() },
        Step { id: Some("eng".into()), text: "VP Engineering".into(), ..Default::default() },
        Step { id: Some("sales".into()), text: "VP Sales".into(), ..Default::default() },
    ];
    let out = generate_accessible_svg(DiagramType::Tree, "Org chart", "Reporting structure for the leadership team.", steps);
    assert_snapshot("tree", &out);
}

#[test]
fn swimlane_snapshot() {
    let steps = vec![
        Step { text: "Place order".into(), lane: Some("Customer".into()), ..Default::default() },
        Step { text: "Charge card".into(), lane: Some("Payments".into()), ..Default::default() },
        Step { text: "Ship item".into(), lane: Some("Warehouse".into()), ..Default::default() },
    ];
    let out = generate_accessible_svg(DiagramType::Swimlane, "Order handoff", "Which team handles each step of an order.", steps);
    assert_snapshot("swimlane", &out);
}

#[test]
fn journey_map_snapshot() {
    let steps = vec![
        Step {
            text: "Discovery".into(),
            subtitle: Some("Reads the product overview".into()),
            sentiment: Some(1),
            ..Default::default()
        },
        Step {
            text: "Sign up".into(),
            subtitle: Some("Fills out the signup form".into()),
            sentiment: Some(-1),
            pain: Some("Too many fields".into()),
            ..Default::default()
        },
        Step { text: "First success".into(), subtitle: Some("Runs their first query".into()), sentiment: Some(2), ..Default::default() },
    ];
    let out = generate_accessible_svg(DiagramType::JourneyMap, "Onboarding journey", "How a new user experiences onboarding.", steps);
    assert_snapshot("journey_map", &out);
}

#[test]
fn chart_snapshot() {
    let categories = vec!["Q1".to_string(), "Q2".to_string(), "Q3".to_string(), "Q4".to_string()];
    let series = vec![ChartSeries { name: "Revenue".to_string(), values: vec![12.4, 15.1, 22.8, 24.0] }];
    let out = generate_accessible_chart(
        ChartType::Bar,
        "Revenue by quarter",
        "Revenue rose each quarter, with the largest jump in Q3.",
        categories,
        series,
        None,
        Some("USD millions"),
    );
    assert_snapshot("chart_bar", &out);
}

#[test]
fn table_snapshot() {
    let headers = vec!["Region".to_string(), "Q1".to_string(), "Q2".to_string()];
    let rows = vec![
        vec![serde_json::json!("West"), serde_json::json!(120), serde_json::json!(130)],
        vec![serde_json::json!("East"), serde_json::json!(90), serde_json::json!(95)],
    ];
    let out = generate_accessible_table("Regional sales", &headers, &rows, Some("Sales rose in both regions."));
    assert_snapshot("table", &out);
}
