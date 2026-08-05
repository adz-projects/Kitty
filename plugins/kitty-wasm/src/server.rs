//! rmcp server exposing the sandbox.
//!
//! `execute_math_python` is kept as the primary tool name even though the
//! implementation no longer resembles `wasm_math_mcp.py`'s: adaptive-pathway
//! keys learned routing on the literal name string, and every existing
//! session, recipe and prompt refers to it (see `docs/PLUGINS.md`). The
//! clearer `wasm_python_run` is registered as an alias so new callers have a
//! name that describes what actually happens.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::sandbox::{self, Mount, RunRequest, MAX_TIMEOUT_SECS};
use crate::{guest, python};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecutePythonRequest {
    /// Python source to execute. Assign to a variable named `result` to
    /// return a structured value; anything printed is captured separately.
    pub code: String,
    /// Optional variables injected as globals before the code runs.
    ///
    /// Typed as a free-form object rather than a schema with boolean
    /// sub-schemas: llama.cpp's grammar compiler rejects `true`/`false` as a
    /// sub-schema value, which is what a permissive `additionalProperties`
    /// would emit. Same workaround as `wasm_math_mcp.py`'s
    /// `_variables_json_schema`.
    pub variables: Option<Map<String, Value>>,
    /// Wall-clock limit in seconds (default 60, max 300).
    pub timeout_s: Option<u64>,
    /// Host directory to mount read-write at `/work`. Nothing outside it is
    /// reachable from inside the sandbox.
    pub workspace: Option<String>,
    /// Download the ~26 MB CPython guest if it isn't installed yet.
    pub install: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunModuleRequest {
    /// Path to a `.wasm` (or `.wat`) module exporting a WASI `_start`.
    pub module_path: String,
    /// Arguments passed as the guest's argv (argv[0] is supplied for you).
    pub args: Option<Vec<String>>,
    /// Host directory to mount read-write at `/work`.
    pub workspace: Option<String>,
    /// Wall-clock limit in seconds (default 60, max 300).
    pub timeout_s: Option<u64>,
    /// Deterministic instruction budget. Omit for wall-clock bounding only.
    pub fuel: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GuestStatusRequest {
    /// Download the guest if it isn't installed yet.
    pub install: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct KittyWasmServer {
    tool_router: ToolRouter<Self>,
}

impl Default for KittyWasmServer {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyWasmServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::wasm_tool_router(),
        }
    }

    /// Sorted list of every registered tool name, pinned by `server.rs`'s
    /// tests. Renaming an entry orphans adaptive-pathway's learned routing.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }
}

fn guarded(f: impl FnOnce() -> String) -> String {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(s) => s,
        Err(_) => error_json(
            "INTERNAL_PANIC",
            "An internal error occurred while processing this request.",
        ),
    }
}

fn error_json(kind: &str, message: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "status": "error",
        "result": null,
        "error": {"error_type": kind, "message": message},
    }))
    .unwrap_or_else(|_| "{\"status\":\"error\"}".to_string())
}

fn clamp_timeout(requested: Option<u64>) -> Duration {
    Duration::from_secs(
        requested
            .unwrap_or(sandbox::DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    )
}

/// Shared by both Python tool names.
async fn execute_python(req: ExecutePythonRequest) -> String {
    let guest_path = match guest::ensure_python_guest(req.install.unwrap_or(false)).await {
        Ok(p) => p,
        Err(e) => return error_json("GuestUnavailable", &format!("{e:#}")),
    };

    let workspace = req.workspace.map(std::path::PathBuf::from);
    if let Some(dir) = &workspace {
        if !dir.is_dir() {
            return error_json(
                "WorkspaceNotFound",
                &format!("workspace is not an existing directory: {}", dir.display()),
            );
        }
    }

    let variables = req.variables.unwrap_or_default();
    let timeout = clamp_timeout(req.timeout_s);
    let code = req.code;

    // `spawn_blocking` is mandatory, not a nicety — see `sandbox::run_module`'s
    // doc comment: the synchronous WASI shim it links uses `block_on`, which
    // panics if it runs on a thread already driving the tokio reactor.
    let joined = tokio::task::spawn_blocking(move || {
        python::run_python(
            &guest_path,
            &code,
            &variables,
            timeout,
            workspace.as_deref(),
        )
    })
    .await;

    match joined {
        Ok(Ok(value)) => serde_json::to_string_pretty(&value)
            .unwrap_or_else(|e| error_json("SerializationError", &e.to_string())),
        Ok(Err(e)) => error_json("SandboxError", &format!("{e:#}")),
        Err(e) => error_json("SandboxPanicked", &format!("sandbox task failed: {e}")),
    }
}

#[tool_router(router = wasm_tool_router)]
impl KittyWasmServer {
    #[tool(
        name = "execute_math_python",
        description = "YOU HAVE A SANDBOXED PYTHON 3 RUNTIME — DO YOUR COMPUTING HERE, NOT IN PROSE. This is the only general-purpose compute tool: real arithmetic, counting, aggregation, statistics, parsing, and regex over raw data. Do NOT total, average, count, rank, transform, or do multi-step math in plain text — you WILL make arithmetic errors. Delegate it here and trust the exact, deterministic output.\n\nIt CANNOT open your files, scraped pages, or the internet alone (no network; only a `workspace` you mount read-write at /work). Get data in by inlining it in the script or injecting objects as globals via the `variables` argument — the clean way to hand it large payloads. The full standard library works (math, statistics, decimal, fractions, collections, itertools, json, re, datetime, and more).\n\nReturn the answer by assigning it to a variable named `result` (or `_last_result`) — it comes back as structured JSON. `print()` is for logs only; stdout is captured separately and is NOT the result.\n\nREACH FOR THIS TOOL AGGRESSIVELY: any arithmetic, counting, data summarization, or text extraction → choose this over computing mentally. Optional: `timeout_s` (default 60, max 300), `workspace`."
    )]
    pub async fn execute_math_python(
        &self,
        Parameters(req): Parameters<ExecutePythonRequest>,
    ) -> String {
        execute_python(req).await
    }

    #[tool(
        name = "wasm_python_run",
        description = "Alias of execute_math_python with a clearer name: YOU HAVE A SANDBOXED PYTHON 3 RUNTIME — DO YOUR COMPUTING HERE, NOT IN PROSE. The only general-purpose compute tool: real arithmetic, counting, aggregation, statistics, parsing, and regex over raw data. Do NOT total, average, count, rank, transform, or do multi-step math in plain text — you WILL make arithmetic errors; delegate here and trust the exact, deterministic output.\n\nIt CANNOT open your files, scraped pages, or the internet alone (no network; only a `workspace` you mount read-write at /work). Get data in by inlining it in the script or injecting objects as globals via the `variables` argument — the clean way to hand it large payloads. Full standard library available (math, statistics, decimal, fractions, collections, itertools, json, re, datetime).\n\nReturn the answer by assigning it to a variable named `result` (or `_last_result`) — it comes back as structured JSON. `print()` is for logs only; stdout is captured separately and is NOT the result.\n\nREACH FOR THIS TOOL AGGRESSIVELY: any arithmetic, counting, data summarization, or text extraction → choose this over computing mentally. Optional: `timeout_s` (default 60, max 300), `workspace`."
    )]
    pub async fn wasm_python_run(
        &self,
        Parameters(req): Parameters<ExecutePythonRequest>,
    ) -> String {
        execute_python(req).await
    }

    #[tool(
        name = "wasm_run_module",
        description = "Runs an arbitrary WebAssembly module (a WASI command exporting _start) in the sandbox, capturing stdout/stderr and the exit code. No network access; the only reachable directory is the optional workspace, mounted at /work. Use this to run a purpose-built .wasm tool; use execute_math_python for ordinary scripting."
    )]
    pub async fn wasm_run_module(&self, Parameters(req): Parameters<RunModuleRequest>) -> String {
        // Blocking sandbox work off the reactor — see `execute_python` above
        // and `sandbox::run_module`'s doc comment for why this is required.
        tokio::task::spawn_blocking(move || guarded(move || {
            let module_path = std::path::PathBuf::from(&req.module_path);
            if !module_path.is_file() {
                return error_json(
                    "ModuleNotFound",
                    &format!("no wasm module at {}", module_path.display()),
                );
            }

            let module = match sandbox::load_module_cached(&module_path, &guest::module_cache_dir())
            {
                Ok(m) => m,
                Err(e) => return error_json("ModuleLoadFailed", &format!("{e:#}")),
            };

            let mut mounts = Vec::new();
            if let Some(workspace) = &req.workspace {
                let dir = std::path::PathBuf::from(workspace);
                if !dir.is_dir() {
                    return error_json(
                        "WorkspaceNotFound",
                        &format!("workspace is not an existing directory: {workspace}"),
                    );
                }
                mounts.push(Mount::writable(dir, "/work"));
            }

            let mut args = vec!["module".to_string()];
            args.extend(req.args.clone().unwrap_or_default());

            let request = RunRequest {
                args,
                mounts,
                timeout: clamp_timeout(req.timeout_s),
                fuel: req.fuel,
                ..Default::default()
            };

            match sandbox::run_module(&module, &request) {
                Ok(output) => {
                    let exit_code = match &output.outcome {
                        sandbox::Outcome::Exited { code } => Some(*code),
                        _ => None,
                    };
                    serde_json::to_string_pretty(&json!({
                        "status": if output.outcome.is_success() { "success" } else { "error" },
                        "outcome": output.outcome.label(),
                        "exit_code": exit_code,
                        "detail": match &output.outcome {
                            sandbox::Outcome::Trapped { message } => Some(message.clone()),
                            _ => None,
                        },
                        "stdout": output.stdout.contents(),
                        "stderr": output.stderr.contents(),
                        "stdout_truncated": output.stdout.truncated(),
                        "execution_time_ms": output.duration.as_millis() as u64,
                    }))
                    .unwrap_or_else(|e| error_json("SerializationError", &e.to_string()))
                }
                Err(e) => error_json("SandboxError", &format!("{e:#}")),
            }
        }))
        .await
        .unwrap_or_else(|e| error_json("SandboxPanicked", &format!("sandbox task failed: {e}")))
    }

    #[tool(
        name = "wasm_guest_status",
        description = "Reports whether the sandboxed Python interpreter is installed, where it resolves from, and how to install it. Pass install=true to download it (~26 MB, pinned and checksum-verified)."
    )]
    pub async fn wasm_guest_status(
        &self,
        Parameters(req): Parameters<GuestStatusRequest>,
    ) -> String {
        if req.install.unwrap_or(false) {
            if let Err(e) = guest::ensure_python_guest(true).await {
                return error_json("GuestInstallFailed", &format!("{e:#}"));
            }
        }
        serde_json::to_string_pretty(&json!({
            "status": "success",
            "data": guest::python_guest_status(),
        }))
        .unwrap_or_else(|e| error_json("SerializationError", &e.to_string()))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KittyWasmServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_surface_is_pinned() {
        // `execute_math_python` in particular must never be renamed — see
        // this module's header.
        assert_eq!(
            KittyWasmServer::new().tool_names(),
            vec![
                "execute_math_python".to_string(),
                "wasm_guest_status".to_string(),
                "wasm_python_run".to_string(),
                "wasm_run_module".to_string(),
            ]
        );
    }

    #[test]
    fn timeout_is_clamped_into_the_documented_range() {
        assert_eq!(clamp_timeout(None).as_secs(), sandbox::DEFAULT_TIMEOUT_SECS);
        assert_eq!(clamp_timeout(Some(0)).as_secs(), 1, "must not allow a zero budget");
        assert_eq!(clamp_timeout(Some(5)).as_secs(), 5);
        assert_eq!(clamp_timeout(Some(99_999)).as_secs(), MAX_TIMEOUT_SECS);
    }

    #[test]
    fn guarded_converts_a_panic_into_structured_json() {
        let out = guarded(|| panic!("boom"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error"]["error_type"], "INTERNAL_PANIC");
    }

    #[tokio::test]
    async fn run_module_reports_a_missing_module_cleanly() {
        let server = KittyWasmServer::new();
        let out = server
            .wasm_run_module(Parameters(RunModuleRequest {
                module_path: "/definitely/not/here.wasm".into(),
                args: None,
                workspace: None,
                timeout_s: None,
                fuel: None,
            }))
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["error_type"], "ModuleNotFound");
    }

    #[tokio::test]
    async fn run_module_rejects_a_workspace_that_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let wat = dir.path().join("m.wat");
        std::fs::write(&wat, "(module (func (export \"_start\")))").unwrap();

        let server = KittyWasmServer::new();
        let out = server
            .wasm_run_module(Parameters(RunModuleRequest {
                module_path: wat.to_string_lossy().into_owned(),
                args: None,
                workspace: Some("/definitely/not/a/dir".into()),
                timeout_s: None,
                fuel: None,
            }))
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["error_type"], "WorkspaceNotFound");
    }

    #[tokio::test]
    async fn run_module_executes_a_real_module_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let wat = dir.path().join("m.wat");
        std::fs::write(
            &wat,
            r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "from the tool\n")
              (func (export "_start")
                (i32.store (i32.const 8) (i32.const 100))
                (i32.store (i32.const 12) (i32.const 14))
                (drop (call $fd_write (i32.const 1) (i32.const 8) (i32.const 1) (i32.const 20)))))
            "#,
        )
        .unwrap();

        let server = KittyWasmServer::new();
        let out = server
            .wasm_run_module(Parameters(RunModuleRequest {
                module_path: wat.to_string_lossy().into_owned(),
                args: None,
                workspace: Some(dir.path().to_string_lossy().into_owned()),
                timeout_s: Some(10),
                fuel: None,
            }))
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success", "{v}");
        assert_eq!(v["outcome"], "exited");
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "from the tool\n");
    }

    #[tokio::test]
    async fn guest_status_is_always_answerable_even_with_no_guest_installed() {
        let server = KittyWasmServer::new();
        let out = server
            .wasm_guest_status(Parameters(GuestStatusRequest { install: Some(false) }))
            .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "success");
        assert!(v["data"]["pinned"]["sha256"].is_string());
    }

    #[test]
    fn python_without_an_installed_guest_explains_itself() {
        use crate::guest::testing::{block_on, env_lock, EnvGuard};

        let dir = tempfile::tempdir().unwrap();
        // Shares `guest`'s lock and guards: env vars are process-global, so
        // hand-rolled save/restore here would still race the tests in that
        // module, and an early panic would leak the override into the rest
        // of the binary.
        let out = {
            let _lock = env_lock();
            let _data = EnvGuard::set("KITTY_WASM_DATA_DIR", dir.path().to_str().unwrap());
            let _no_override = EnvGuard::unset("KITTY_WASM_PYTHON");
            block_on(execute_python(ExecutePythonRequest {
                code: "result = 1".into(),
                variables: None,
                timeout_s: None,
                workspace: None,
                install: Some(false),
            }))
        };

        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"]["error_type"], "GuestUnavailable");
        assert!(v["error"]["message"].as_str().unwrap().contains("install=true"));
    }
}
