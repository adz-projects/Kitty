//! Minimal stdio MCP server used only by `tests/mcp_never_throws.rs` to
//! exercise `MCPManager::execute_tool`'s never-throws contract against a real
//! subprocess (unknown tool, bad args, timeout, and a crash mid-call).
//! Speaks newline-delimited JSON-RPC directly (no `rmcp` dependency needed on
//! the server side) — just enough of the MCP protocol for `rmcp`'s client to
//! complete `initialize` / `tools/list` / `tools/call`.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn write_line(stdout: &mut impl Write, value: &Value) {
    let s = serde_json::to_string(value).unwrap();
    let _ = writeln!(stdout, "{s}");
    let _ = stdout.flush();
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                if let Some(id) = id {
                    write_line(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": {},
                                "serverInfo": {"name": "fake-mcp-server", "version": "0.1.0"}
                            }
                        }),
                    );
                }
            }
            "notifications/initialized" => {}
            "tools/list" => {
                if let Some(id) = id {
                    write_line(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [
                                    {
                                        "name": "echo_tool",
                                        "description": "Echoes text back",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {"text": {"type": "string"}},
                                            "required": ["text"]
                                        }
                                    },
                                    {
                                        "name": "sleep_tool",
                                        "description": "Sleeps for N milliseconds before responding",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {"millis": {"type": "number"}},
                                            "required": ["millis"]
                                        }
                                    },
                                    {
                                        "name": "crash_tool",
                                        "description": "Exits the process immediately, mid-call",
                                        "inputSchema": {"type": "object", "properties": {}}
                                    }
                                ]
                            }
                        }),
                    );
                }
            }
            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                match name {
                    "echo_tool" => {
                        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(id) = id {
                            write_line(
                                &mut stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{"type": "text", "text": text}],
                                        "isError": false
                                    }
                                }),
                            );
                        }
                    }
                    "sleep_tool" => {
                        let millis = args.get("millis").and_then(|v| v.as_u64()).unwrap_or(0);
                        std::thread::sleep(std::time::Duration::from_millis(millis));
                        if let Some(id) = id {
                            write_line(
                                &mut stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{"type": "text", "text": "woke up"}],
                                        "isError": false
                                    }
                                }),
                            );
                        }
                    }
                    "crash_tool" => {
                        std::process::exit(1);
                    }
                    _ => {
                        if let Some(id) = id {
                            write_line(
                                &mut stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": {"code": -32601, "message": format!("Unknown tool: {name}")}
                                }),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
