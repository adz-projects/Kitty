//! The decision model, applied live.
//!
//! The engine keeps one decision-model configuration per app
//! (`/api/apps/me/decider`). This module turns Kitty's `DeciderSettings` into
//! that document — adding Kitty's own tool-group catalog, which only Kitty can
//! know — and PUTs it whenever the engine's copy differs. kitty-web gets the
//! same settings through its MCP row (`bigtiny::mcp::ensure_builtin_servers`),
//! since its checks run in the plugin, not the engine.
//!
//! Called after every attach, after a settings change, and from the
//! self-heal, so an engine restarted by another app catches up on its own.

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::config::DeciderSettings;
use crate::state::AppState;

/// Kitty's tools, grouped by what a request needs. Tools in no group (the
/// memory tools, the specialists) are never gated.
///
/// `core` is listed for the record: it is `always`, so it is never gated
/// either, but naming it keeps the catalog an honest map of the tool surface.
pub fn tool_groups() -> Value {
    json!([
        {
            "name": "core",
            "description": "working with files, the workspace, notes or shell commands",
            "tools": ["lean_analyze_workspace", "lean_file_*", "lean_scratchpad_*",
                      "lean_cache_*", "lean_doc_*", "lean_shell*"],
            "always": true,
        },
        {
            "name": "office",
            "description": "reading or writing Word, Excel or PDF documents",
            "tools": ["lean_word_*", "lean_excel_*", "lean_pdf_*"],
            // An attached file of these types loads the group outright.
            "file_types": [".docx", ".doc", ".xlsx", ".xls", ".xlsm", ".csv", ".pdf"],
        },
        {
            "name": "image",
            "description": "looking at an image or screenshot file",
            "tools": ["lean_read_image"],
            "file_types": [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"],
        },
        {
            "name": "viz",
            "description": "drawing a chart, diagram, table or Mermaid graph",
            "tools": ["generate_accessible_*"],
        },
        {
            "name": "web",
            "description": "searching the web or reading a web page or online document",
            "tools": ["lean_web_*"],
        },
        {
            "name": "code_exec",
            "description": "running Python or other code to compute or test something",
            "tools": ["wasm_*"],
            // Always on: the chat model reaches for Python to compute things
            // no message asks for in so many words (power and noise estimates,
            // a deploy script) -- four of nine misses in a replay of 17 real
            // chats, for three tool schemas.
            "always": true,
        },
    ])
}

/// The document the engine should hold, or `None` for "no decision model".
/// Pure, so the mapping is testable.
pub fn desired(settings: &DeciderSettings) -> Option<Value> {
    if !settings.configured() {
        return None;
    }
    let f = &settings.features;
    Some(json!({
        "dialect": settings.dialect,
        "url": settings.url.trim(),
        "model": settings.model,
        // The two web features run in kitty-web, which reads its own copy;
        // they are sent here too so the engine's view of the app is complete.
        "features": {
            "tool_gating": f.tool_gating,
            "learn_gate": f.learn_gate,
            "recall_rerank": f.recall_rerank,
            "ingest_gate": f.ingest_gate,
            "page_classify": f.page_classify,
            "search_rerank": f.search_rerank,
            "compaction": f.compaction,
            "shell_screen": f.shell_screen,
        },
        "thresholds": Value::Object(settings.thresholds.clone()),
        "tool_groups": tool_groups(),
    }))
}

/// Bring the engine's copy in line with Settings. Best-effort: failures are
/// logged and leave the engine as it was (which fails open by design).
pub async fn apply_decider(app: &AppHandle) {
    if let Err(e) = apply(app).await {
        tracing::warn!("could not apply the decision-model settings: {e}");
    }
}

async fn apply(app: &AppHandle) -> Result<(), String> {
    let client = crate::bigtiny::client::ensure_client(app)?;
    let want = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        desired(&cfg.decider)
    };
    let have = client.get_json("/api/apps/me/decider").await?;
    let have = have.get("config").cloned().unwrap_or(Value::Null);
    match want {
        Some(want) if !same(&have, &want) => {
            client.put_json("/api/apps/me/decider", &want).await?;
        }
        None if !have.is_null() => {
            client.delete("/api/apps/me/decider").await?;
        }
        _ => {}
    }
    Ok(())
}

/// Whether the engine's stored document already says what we want. The
/// engine echoes the thresholds back with every default filled in, so those
/// are compared by the keys we set rather than wholesale.
fn same(have: &Value, want: &Value) -> bool {
    let keys = ["dialect", "url", "model", "features"];
    if keys.iter().any(|k| have.get(k) != want.get(k)) {
        return false;
    }
    // The engine echoes every group with its defaults filled in (`always:
    // false`, `file_types: []`), which the catalog leaves out; compared raw,
    // the two never matched and every self-heal re-sent the settings.
    if groups_with_defaults(have.get("tool_groups")) != groups_with_defaults(want.get("tool_groups")) {
        return false;
    }
    let empty = serde_json::Map::new();
    let want_t = want["thresholds"].as_object().unwrap_or(&empty);
    want_t
        .iter()
        .all(|(k, v)| have["thresholds"].get(k).and_then(Value::as_f64) == v.as_f64())
}

/// Tool groups with the engine's defaults filled in, for comparison.
fn groups_with_defaults(groups: Option<&Value>) -> Vec<Value> {
    groups
        .and_then(Value::as_array)
        .map(|gs| {
            gs.iter()
                .map(|g| {
                    let mut g = g.clone();
                    if let Some(obj) = g.as_object_mut() {
                        obj.entry("always").or_insert(json!(false));
                        obj.entry("file_types").or_insert(json!([]));
                    }
                    g
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's echo of the catalog, as it serializes `ToolGroup`: every
    /// field present. Kitty must recognise it as what it sent.
    #[test]
    fn the_engines_echo_of_the_catalog_counts_as_the_same() {
        let s = DeciderSettings {
            url: "http://box:8080".into(),
            ..DeciderSettings::default()
        };
        let want = desired(&s).unwrap();
        let mut have = want.clone();
        for g in have["tool_groups"].as_array_mut().unwrap() {
            let obj = g.as_object_mut().unwrap();
            obj.entry("always").or_insert(json!(false));
            obj.entry("file_types").or_insert(json!([]));
        }
        have["cache_ttl_secs"] = json!(300);
        assert!(same(&have, &want));
        have["tool_groups"][1]["file_types"] = json!([".pdf"]);
        assert!(!same(&have, &want));
    }

    #[test]
    fn no_url_means_no_decision_model() {
        assert!(desired(&DeciderSettings::default()).is_none());
        let s = DeciderSettings {
            url: "  ".into(),
            ..DeciderSettings::default()
        };
        assert!(desired(&s).is_none());
    }

    #[test]
    fn the_document_carries_every_switch_and_the_catalog() {
        let mut s = DeciderSettings {
            url: "http://box:8080 ".into(),
            ..DeciderSettings::default()
        };
        s.features.compaction = false;
        let d = desired(&s).unwrap();
        assert_eq!(d["url"], "http://box:8080");
        assert_eq!(d["features"]["compaction"], false);
        assert_eq!(d["features"]["tool_gating"], true);
        assert!(d["tool_groups"].as_array().unwrap().len() >= 5);
        let group = |name: &str| {
            d["tool_groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["name"] == name)
                .cloned()
                .unwrap()
        };
        assert_eq!(group("code_exec")["always"], true);
        assert!(group("office")["file_types"]
            .as_array()
            .unwrap()
            .contains(&json!(".docx")));
    }

    #[test]
    fn an_echo_with_filled_in_defaults_counts_as_the_same() {
        let mut s = DeciderSettings {
            url: "http://box:8080".into(),
            ..DeciderSettings::default()
        };
        s.thresholds
            .insert("tool_gating_add".into(), json!(0.75));
        let want = desired(&s).unwrap();
        let mut have = want.clone();
        have["thresholds"] = json!({"tool_gating_add": 0.75, "tool_gating_cold": 0.3});
        assert!(same(&have, &want));
        have["thresholds"]["tool_gating_add"] = json!(0.6);
        assert!(!same(&have, &want));
    }
}
