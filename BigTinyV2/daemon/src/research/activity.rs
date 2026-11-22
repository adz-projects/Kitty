//! What a run is doing right now, for a client to show between milestones.
//!
//! A wave takes minutes, and its milestones (planning started, findings in)
//! are minutes apart. In between, the only honest signal of life is the work
//! itself: which planner or researcher is running and what tool it last
//! called. That is read from the database (every session saves its messages
//! as it goes), so it needs no hook into the agent loop and survives a client
//! reconnecting. The step and the last moment anything happened are kept in
//! memory here, per run.

use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::storage::messages;

/// Most workers listed in one report.
const MAX_WORKERS: usize = 12;

#[derive(Debug, Clone, Default)]
struct Live {
    /// The current step, as the progress event's `step`.
    step: Option<Value>,
    step_started_ms: i64,
    last_activity_ms: i64,
}

static LIVE: Lazy<DashMap<String, Live>> = Lazy::new(DashMap::new);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A new step began.
pub fn set_step(run_id: &str, step: Value) {
    let now = now_ms();
    let mut e = LIVE.entry(run_id.to_string()).or_default();
    e.step = Some(step);
    e.step_started_ms = now;
    e.last_activity_ms = now;
}

/// Something happened (a delegate finished, findings were recorded).
pub fn touch(run_id: &str) {
    let now = now_ms();
    LIVE.entry(run_id.to_string()).or_default().last_activity_ms = now;
}

/// The run ended: forget it.
pub fn clear(run_id: &str) {
    LIVE.remove(run_id);
}

/// One planner or researcher at work.
#[derive(Debug, Clone, PartialEq)]
pub struct Worker {
    pub session_id: String,
    pub label: String,
    pub doing: Option<String>,
    pub started_ms: i64,
    pub last_ms: i64,
}

/// What the run is doing now, as JSON for the progress event: the step,
/// when it started, the last moment anything happened, and the sessions at
/// work under `chat`. `stall_after_secs` is how long silence can last before
/// a client should call it stuck.
pub async fn snapshot(
    pool: &SqlitePool,
    run_id: &str,
    chat: Option<&str>,
    stall_after_secs: u64,
) -> Value {
    let live = LIVE.get(run_id).map(|e| e.clone()).unwrap_or_default();
    let workers = match chat {
        Some(chat) => workers(pool, chat).await,
        None => vec![],
    };
    let last = workers
        .iter()
        .map(|w| w.last_ms)
        .chain(std::iter::once(live.last_activity_ms))
        .max()
        .unwrap_or(0);
    json!({
        "step": live.step,
        "step_started_ms": (live.step_started_ms > 0).then_some(live.step_started_ms),
        "last_activity_ms": (last > 0).then_some(last),
        "stall_after_secs": stall_after_secs,
        "workers": workers.iter().map(|w| json!({
            "session_id": w.session_id,
            "label": w.label,
            "doing": w.doing,
            "started_ms": w.started_ms,
            "last_ms": w.last_ms,
        })).collect::<Vec<_>>(),
    })
}

/// The sessions working for a run right now: the chat's children with a
/// turn in progress, each with what it last did.
async fn workers(pool: &SqlitePool, chat: &str) -> Vec<Worker> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, created_at FROM sessions \
         WHERE parent_session_id = ? AND status = 'active' \
         ORDER BY created_at, rowid LIMIT ?",
    )
    .bind(chat)
    .bind(MAX_WORKERS as i64)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    for (session_id, name, created_at) in rows {
        let started_ms = parse_sqlite_time(&created_at).unwrap_or(0);
        let recent = messages::get_last_messages_by_session(pool, &session_id, 6)
            .await
            .unwrap_or_default();
        let last_ms = recent
            .iter()
            .filter_map(|m| m.created_at.map(|t| t.timestamp_millis()))
            .max()
            .unwrap_or(started_ms);
        let doing = recent
            .iter()
            .filter(|m| m.role == "assistant")
            .max_by_key(|m| m.rowid)
            .and_then(|m| m.tool_calls.as_deref())
            .and_then(describe_tool_calls);
        out.push(Worker {
            session_id,
            label: label_of(&name),
            doing,
            started_ms,
            last_ms,
        });
    }
    out
}

/// `2026-11-22 01:03:41` (SQLite's `datetime('now')`, UTC) in epoch ms.
fn parse_sqlite_time(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|t| t.and_utc().timestamp_millis())
}

/// A worker's name for people: a delegate session is named
/// `<specialist>: <start of its prompt>`, a planner `Research wave N: <title>`.
pub fn label_of(session_name: &str) -> String {
    if let Some(rest) = session_name.strip_prefix("Research wave ") {
        let wave = rest.split(':').next().unwrap_or("").trim();
        return format!("Planning wave {wave}");
    }
    let (who, what) = session_name.split_once(':').unwrap_or(("", session_name));
    let what = what.trim().trim_start_matches("Objective:").trim();
    let what = what.trim_end_matches("...").trim_end_matches('…').trim();
    let label = if who.trim().is_empty() {
        what.to_string()
    } else {
        format!("{}: {what}", who.trim())
    };
    crate::research::ledger::clip(&label, 90)
}

/// What a step's tool calls amount to, in a few words: the first call
/// described (the rest of a parallel batch are usually more of the same).
pub fn describe_tool_calls(tool_calls: &str) -> Option<String> {
    let calls: Vec<Value> = serde_json::from_str(tool_calls).ok()?;
    let first = calls.first()?;
    let f = first.get("function").unwrap_or(first);
    let name = f.get("name")?.as_str()?;
    let args = match f.get("arguments") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    };
    let more = if calls.len() > 1 {
        format!(" (+{} more)", calls.len() - 1)
    } else {
        String::new()
    };
    Some(format!("{}{more}", describe(name, &args)))
}

fn describe(tool: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim);
    let clip = |t: &str| crate::research::ledger::clip(t, 70);
    match tool {
        "lean_web_search" => match s("query") {
            Some(q) => format!("searching: {}", clip(q)),
            None => "searching the web".into(),
        },
        "lean_web_search_read_chunk" => "reading search results".into(),
        "lean_web_scrape" => match s("url").and_then(crate::research::ledger::domain_of) {
            Some(d) => format!("reading {d}"),
            None => "reading a page".into(),
        },
        "lean_web_find_document" => match s("title") {
            Some(t) => format!("looking for a document: {}", clip(t)),
            None => "looking for a document".into(),
        },
        "wasm_python_run" | "execute_math_python" => "computing".into(),
        crate::research::server::VIEW_TOOL => "reading the ledger".into(),
        crate::research::server::RECORD_TOOL => "recording leads and links".into(),
        crate::research::server::FRONTIER_TOOL => "setting the frontier".into(),
        t if t.starts_with("lean_doc_")
            || t.starts_with("lean_pdf_")
            || t.starts_with("lean_word_")
            || t == "lean_file_read"
            || t.starts_with("lean_excel_") =>
        {
            "reading a document".into()
        }
        other => format!("using {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls_read_as_what_the_worker_is_doing() {
        let search = r#"[{"id":"c1","type":"function","function":{"name":"lean_web_search","arguments":{"query":"\"Frank Johnson\" Los Angeles","count":"10"}}},
                         {"id":"c2","type":"function","function":{"name":"lean_web_search","arguments":{"query":"x"}}}]"#;
        assert_eq!(
            describe_tool_calls(search).as_deref(),
            Some("searching: \"Frank Johnson\" Los Angeles (+1 more)")
        );
        let scrape = r#"[{"function":{"name":"lean_web_scrape","arguments":"{\"url\":\"https://www.sos.ca.gov/x\"}"}}]"#;
        assert_eq!(
            describe_tool_calls(scrape).as_deref(),
            Some("reading sos.ca.gov")
        );
        let frontier = r#"[{"function":{"name":"research_set_frontier","arguments":{}}}]"#;
        assert_eq!(
            describe_tool_calls(frontier).as_deref(),
            Some("setting the frontier")
        );
        assert_eq!(describe_tool_calls("not json"), None);
        assert_eq!(describe_tool_calls("[]"), None);
    }

    #[test]
    fn session_names_read_as_labels() {
        assert_eq!(
            label_of("Research wave 2: Exhaustive public records"),
            "Planning wave 2"
        );
        assert_eq!(
            label_of("researcher: Objective: CA SOS entity registrations..."),
            "researcher: CA SOS entity registrations"
        );
        assert_eq!(label_of("plain"), "plain");
    }

    #[test]
    fn steps_and_activity_are_remembered_per_run() {
        set_step("r-act", json!({"step": "planning", "wave": 1}));
        let started = LIVE.get("r-act").unwrap().step_started_ms;
        assert!(started > 0);
        touch("r-act");
        assert!(LIVE.get("r-act").unwrap().last_activity_ms >= started);
        clear("r-act");
        assert!(LIVE.get("r-act").is_none());
    }

    #[test]
    fn sqlite_times_are_utc() {
        assert_eq!(parse_sqlite_time("1970-01-01 00:00:01"), Some(1000));
        assert_eq!(parse_sqlite_time("nope"), None);
    }
}
