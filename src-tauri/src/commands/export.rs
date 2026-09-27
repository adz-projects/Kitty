//! ChatML export (Phase 11, decision #25): a `.chatml` file anyone can read as
//! plain text, and a `.meta.json` beside it with what ChatML has no room for -
//! the provider and model of every turn, the working folder, timestamps, and
//! every tool call in full.
//!
//! Built from the daemon's stored history rather than whatever a window has
//! on screen, so an export of a live chat and of one resumed later are the
//! same file, and exporting many chats needs none of them loaded.
//!
//! ```text
//! <|im_start|>system
//! <system prompt><|im_end|>
//! <|im_start|>user
//! <message><|im_end|>
//! <|im_start|>assistant
//! <think>
//! <reasoning, when there is any>
//! </think>
//! [tool_call: lean_file_read → see meta.json#tool_calls[0]]
//! <answer><|im_end|>
//! ```

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tauri::AppHandle;

/// One exported conversation: the ChatML text and its sidecar.
#[derive(Debug, PartialEq)]
pub(crate) struct Export {
    pub chatml: String,
    pub meta: Value,
}

/// Build the export of `rows` (history, oldest first). `keep` limits it to
/// the first `keep` chat bubbles ("Export from here"), counted the way
/// branching counts them.
pub(crate) fn build(
    session_id: &str,
    title: &str,
    metadata: &Value,
    rows: &[Value],
    keep: Option<i64>,
) -> Result<Export, String> {
    let rows: Vec<&Value> = match keep {
        Some(k) => {
            let cut = crate::bigtiny::sessions::truncate_target(rows, k)?;
            match cut {
                Some(last) => {
                    let end = rows
                        .iter()
                        .position(|r| r.get("id").and_then(|v| v.as_str()) == Some(last.as_str()))
                        .map_or(rows.len(), |i| i + 1);
                    rows[..end].iter().collect()
                }
                None => rows.iter().collect(),
            }
        }
        None => rows.iter().collect(),
    };

    let text = |r: &Value| crate::bigtiny::sessions::extract_text(r);
    let str_of = |r: &Value, k: &str| {
        r.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let mut out = String::new();
    let mut turns: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(system) = metadata
        .get("persona_override")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        out.push_str(&format!("<|im_start|>system\n{system}<|im_end|>\n"));
        turns.push(json!({ "index": turns.len(), "role": "system" }));
    }

    let mut i = 0;
    while i < rows.len() {
        let row = rows[i];
        match row.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "user" => {
                out.push_str(&format!("<|im_start|>user\n{}<|im_end|>\n", text(row)));
                turns.push(json!({
                    "index": turns.len(),
                    "role": "user",
                    "created_at": row.get("created_at"),
                }));
                i += 1;
            }
            "assistant" | "tool" => {
                // One assistant turn: every assistant and tool row until the
                // next user message.
                let turn_index = turns.len();
                let mut reasoning: Vec<String> = Vec::new();
                let mut body: Vec<String> = Vec::new();
                let mut provider = None;
                let mut model = None;
                let created_at = row.get("created_at").cloned();
                while i < rows.len()
                    && matches!(
                        rows[i].get("role").and_then(|r| r.as_str()),
                        Some("assistant" | "tool" | "system")
                    )
                {
                    let r = rows[i];
                    match r.get("role").and_then(|v| v.as_str()) {
                        Some("assistant") => {
                            if let Some(t) = str_of(r, "reasoning") {
                                reasoning.push(t);
                            }
                            provider = str_of(r, "provider_id").or(provider);
                            model = str_of(r, "model").or(model);
                            for call in crate::bigtiny::sessions::parse_tool_calls(r) {
                                let n = tool_calls.len();
                                let name = call
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("tool")
                                    .to_string();
                                body.push(format!(
                                    "[tool_call: {name} → see meta.json#tool_calls[{n}]]"
                                ));
                                tool_calls.push(json!({
                                    "index": n,
                                    "turn": turn_index,
                                    "id": call.get("toolCallId"),
                                    "name": name,
                                    "arguments": call.get("rawInput"),
                                    "result": Value::Null,
                                    "is_error": Value::Null,
                                }));
                            }
                            let t = text(r);
                            if !t.trim().is_empty() {
                                body.push(t);
                            }
                        }
                        Some("tool") => {
                            let id = r.get("tool_call_id").and_then(|v| v.as_str());
                            let result = text(r);
                            if let Some(entry) = tool_calls
                                .iter_mut()
                                .rev()
                                .find(|c| c["id"].as_str().is_some() && c["id"].as_str() == id)
                            {
                                entry["is_error"] =
                                    json!(crate::bigtiny::stream::tool_result_is_error(&result));
                                entry["result"] = Value::String(result);
                            }
                        }
                        _ => {} // system rows are internal
                    }
                    i += 1;
                }
                out.push_str("<|im_start|>assistant\n");
                if !reasoning.is_empty() {
                    out.push_str(&format!("<think>\n{}\n</think>\n", reasoning.join("\n\n")));
                }
                out.push_str(&body.join("\n\n"));
                out.push_str("<|im_end|>\n");
                turns.push(json!({
                    "index": turn_index,
                    "role": "assistant",
                    "provider_id": provider,
                    "model": model,
                    "created_at": created_at,
                }));
            }
            _ => i += 1,
        }
    }

    let meta = json!({
        "format": "kitty-chatml-meta/1",
        "session_id": session_id,
        "title": title,
        "cwd": metadata.get("cwd"),
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "truncated_to_bubbles": keep,
        "turns": turns,
        "tool_calls": tool_calls,
    });
    Ok(Export { chatml: out, meta })
}

/// A title as a file name: path separators and reserved characters out.
pub(crate) fn file_stem(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "kitty-chat".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

/// `chat.chatml` -> `chat.meta.json`.
fn meta_path(chatml: &Path) -> PathBuf {
    chatml.with_extension("meta.json")
}

/// Export chats as ChatML. One chat and a `.chatml` destination: written
/// there (with its `.meta.json` beside it). Otherwise `dest` is a folder and
/// each chat is written into it under its title. `keep` ("Export from here")
/// applies only to a single chat. Returns the `.chatml` paths written.
#[tauri::command]
pub async fn export_chatml(
    app: AppHandle,
    session_ids: Vec<String>,
    keep: Option<i64>,
    dest: String,
) -> Result<Vec<String>, String> {
    let client = crate::bigtiny::client::ensure_client(&app)?;
    let single_file = session_ids.len() == 1 && dest.to_ascii_lowercase().ends_with(".chatml");
    let mut written = Vec::new();
    let mut used = std::collections::HashSet::new();
    for id in &session_ids {
        let history = client
            .get_json_long(&format!("/api/chat/{id}/history?limit=10000"))
            .await?;
        let rows = history.as_array().cloned().unwrap_or_default();
        let session = client
            .get_json(&format!("/api/chat/{id}"))
            .await
            .unwrap_or(Value::Null);
        let title = session
            .get("name")
            .or_else(|| session.get("session").and_then(|s| s.get("name")))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("New Chat")
            .to_string();
        let metadata = crate::bigtiny::sessions::metadata(&client, id).await;
        let export = build(
            id,
            &title,
            &metadata,
            &rows,
            if single_file { keep } else { None },
        )?;

        let path = if single_file {
            PathBuf::from(&dest)
        } else {
            let mut stem = file_stem(&title);
            if !used.insert(stem.clone()) {
                stem = format!("{stem}-{}", &id[..id.len().min(8)]);
                used.insert(stem.clone());
            }
            Path::new(&dest).join(format!("{stem}.chatml"))
        };
        let meta = serde_json::to_string_pretty(&export.meta).map_err(|e| e.to_string())?;
        let (p1, p2) = (path.clone(), meta_path(&path));
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = p1.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p1, export.chatml)?;
            std::fs::write(&p2, meta)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        written.push(path.to_string_lossy().into_owned());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Value> {
        vec![
            json!({"id": "u1", "role": "user", "content": "Read a.txt", "created_at": "t1"}),
            json!({
                "id": "a1", "role": "assistant", "content": "",
                "reasoning": "I should read it.",
                "provider_id": "p1", "model": "m1",
                "tool_calls": "[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"lean_file_read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]",
            }),
            json!({"id": "t1", "role": "tool", "tool_call_id": "c1", "content": "hello"}),
            json!({"id": "a2", "role": "assistant", "content": "It says hello.", "provider_id": "p1", "model": "m1"}),
            json!({"id": "u2", "role": "user", "content": "Thanks"}),
            json!({"id": "a3", "role": "assistant", "content": "You're welcome.", "provider_id": "p2", "model": "m2"}),
        ]
    }

    #[test]
    fn a_chat_with_reasoning_and_tools_exports_both_files() {
        let meta = json!({"persona_override": "Be brief.", "cwd": "C:/chats/c1"});
        let e = build("s1", "Chat", &meta, &rows(), None).unwrap();
        assert!(e
            .chatml
            .starts_with("<|im_start|>system\nBe brief.<|im_end|>\n"));
        assert!(e.chatml.contains("<think>\nI should read it.\n</think>\n"));
        assert!(e
            .chatml
            .contains("[tool_call: lean_file_read → see meta.json#tool_calls[0]]"));
        assert!(e.chatml.contains("It says hello.<|im_end|>"));
        assert_eq!(e.meta["tool_calls"][0]["result"], "hello");
        assert_eq!(e.meta["tool_calls"][0]["arguments"]["path"], "a.txt");
        // Per-turn provider and model: the chat changed model part way.
        let assistants: Vec<&Value> = e.meta["turns"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["role"] == "assistant")
            .collect();
        assert_eq!(assistants[0]["model"], "m1");
        assert_eq!(assistants[1]["model"], "m2");
        assert_eq!(e.meta["cwd"], "C:/chats/c1");
    }

    #[test]
    fn no_reasoning_means_no_think_block() {
        let plain = vec![
            json!({"id": "u", "role": "user", "content": "hi"}),
            json!({"id": "a", "role": "assistant", "content": "hello"}),
        ];
        let e = build("s", "t", &Value::Null, &plain, None).unwrap();
        assert!(!e.chatml.contains("<think>"));
        assert!(!e.chatml.contains("<|im_start|>system"));
    }

    #[test]
    fn export_from_here_is_truncated() {
        let e = build("s", "t", &Value::Null, &rows(), Some(2)).unwrap();
        assert!(e.chatml.contains("It says hello."));
        assert!(!e.chatml.contains("Thanks"));
        assert_eq!(e.meta["truncated_to_bubbles"], 2);
    }

    #[test]
    fn titles_become_safe_file_names() {
        assert_eq!(file_stem("a/b: c?"), "a_b_ c_");
        assert_eq!(file_stem("   "), "kitty-chat");
        assert_eq!(
            meta_path(Path::new("x/chat.chatml")),
            PathBuf::from("x/chat.meta.json")
        );
    }
}
