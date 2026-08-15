//! Running Python inside the sandbox, preserving `wasm_math_mcp.py`'s
//! `execute_math_python` response contract.
//!
//! ## How user code reaches the guest
//!
//! Not via `-c`: the code would have to be escaped into an argv string, and
//! any escaping bug becomes a correctness *and* security problem. Instead a
//! per-invocation temp directory is mounted read-only at `/kitty` containing
//! `code.py` and `vars.json`, and a small fixed harness (which never varies,
//! so it has no escaping surface at all) reads them.
//!
//! ## How results come back
//!
//! Through a **file**, `/kitty-out/result.json`, not through stdout or
//! stderr. Both of those are byte-capped ring buffers (see `capture.rs`), and
//! routing the envelope through one means a large result gets truncated
//! *mid-JSON* and becomes unparseable — turning "your result was too big"
//! into "your run mysteriously produced nothing". Writing to a mounted file
//! decouples the envelope from the diagnostic streams entirely, and lets the
//! harness cap the result *inside the guest*, before megabytes are shipped
//! across the boundary only to be discarded.
//!
//! stdout and stderr therefore carry exactly what the user's own code wrote,
//! and nothing else.
//!
//! ## What replaced the AST validator
//!
//! The Python original gated execution on an AST allowlist and a curated
//! `SAFE_GLOBALS`. None of that is ported: `import os; os.system(...)` is
//! simply not dangerous here, because the guest has no OS to talk to. See
//! `sandbox.rs` for the capability table that took its place.

use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::guest;
use crate::sandbox::{self, Mount, Outcome, RunRequest};

/// Byte cap on submitted source, matching `MAX_CODE_LENGTH_BYTES`.
pub const MAX_CODE_LENGTH_BYTES: usize = 50_000;
/// Cap on the serialized `result` value, matching `MAX_RESULT_BYTES`.
pub const MAX_RESULT_BYTES: usize = 256_000;
/// Hard cap on the `result.json` envelope file the host reads back (audit
/// #121). The guest caps the result itself at `MAX_RESULT_BYTES`, but the
/// guest is the adversarial half of this boundary: a buggy or hostile guest
/// could fill `/kitty-out` with gigabytes, and an uncapped `read_to_string`
/// would happily buffer all of it on the host. Well above any legitimate
/// envelope (capped result + preview + error fields).
const RESULT_FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Where the harness writes its envelope, inside the guest. Referenced by
/// the harness source below (as a literal, since that's Python) and pinned by
/// a test that keeps the two in sync.
#[cfg_attr(not(test), allow(dead_code))]
const RESULT_PATH: &str = "/kitty-out/result.json";

/// The fixed harness. Contains no interpolation — every input arrives via the
/// mounted files — so there is nothing here for user code to escape out of.
///
/// `MAX_RESULT_BYTES` is duplicated as a literal because this is a Python
/// source string, not Rust; `python::MAX_RESULT_BYTES` is the authority and a
/// test asserts the two agree.
const HARNESS: &str = r#"
import json, sys, traceback

_KITTY_MAX_RESULT_BYTES = 256000
_kitty_envelope = {"status": "success", "result": None, "error": None}

def _kitty_emit(envelope):
    try:
        payload = json.dumps(envelope, default=str)
    except BaseException:
        payload = json.dumps({
            "status": "error", "result": None,
            "error": {"error_type": "SerializationError",
                      "message": "result could not be serialized to JSON",
                      "line": None, "source_line": None},
        })
    with open("/kitty-out/result.json", "w", encoding="utf-8") as f:
        f.write(payload)

try:
    with open("/kitty/code.py", "r", encoding="utf-8") as _f:
        _kitty_code = _f.read()
    with open("/kitty/vars.json", "r", encoding="utf-8") as _f:
        _kitty_vars = json.load(_f)
except BaseException as _e:
    _kitty_emit({
        "status": "error", "result": None,
        "error": {"error_type": type(_e).__name__, "message": str(_e),
                  "line": None, "source_line": None},
    })
    sys.exit(0)

_kitty_globals = {"__name__": "__main__"}
_kitty_globals.update(_kitty_vars)

try:
    exec(compile(_kitty_code, "<user_code>", "exec"), _kitty_globals)
    _kitty_envelope["result"] = _kitty_globals.get("result", _kitty_globals.get("_last_result"))
except BaseException as _e:
    _tb = _e.__traceback__
    _line = None
    while _tb is not None:
        if _tb.tb_frame.f_code.co_filename == "<user_code>":
            _line = _tb.tb_lineno
        _tb = _tb.tb_next
    _src = None
    if _line is None and isinstance(_e, SyntaxError):
        _line = _e.lineno
    if _line is not None:
        _lines = _kitty_code.splitlines()
        if 0 < _line <= len(_lines):
            _src = _lines[_line - 1].strip()
    _kitty_envelope["status"] = "error"
    _kitty_envelope["result"] = None
    _kitty_envelope["error"] = {
        "error_type": type(_e).__name__,
        "message": str(_e),
        "line": _line,
        "source_line": _src,
        "traceback": "".join(traceback.format_exception_only(type(_e), _e)).strip(),
    }

# Cap the result here, in the guest, rather than shipping megabytes across the
# boundary just for the host to discard them.
try:
    _serialized = json.dumps(_kitty_envelope["result"], default=str)
    if len(_serialized.encode("utf-8")) > _KITTY_MAX_RESULT_BYTES:
        _kitty_envelope["result_size_bytes"] = len(_serialized.encode("utf-8"))
        _kitty_envelope["result_truncated"] = True
        _kitty_envelope["result"] = {
            "truncated": True,
            "reason": "result serialized to %d bytes, over the %d-byte cap" % (
                len(_serialized.encode("utf-8")), _KITTY_MAX_RESULT_BYTES),
            "preview": _serialized[:2000],
        }
    else:
        _kitty_envelope["result_size_bytes"] = len(_serialized.encode("utf-8"))
        _kitty_envelope["result_truncated"] = False
except BaseException:
    _kitty_envelope["result_size_bytes"] = 0
    _kitty_envelope["result_truncated"] = False

sys.stdout.flush()
sys.stderr.flush()
_kitty_emit(_kitty_envelope)
"#;

fn truncate_result(value: Value) -> (Value, bool, usize) {
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    let size = serialized.len();
    if size <= MAX_RESULT_BYTES {
        return (value, false, size);
    }
    // Deliberately replaced wholesale rather than sliced: a truncated JSON
    // fragment is not JSON, and handing the model malformed structure is
    // worse than telling it plainly that the value was too large.
    (
        json!({
            "truncated": true,
            "reason": format!(
                "result serialized to {size} bytes, over the {MAX_RESULT_BYTES}-byte cap"
            ),
            "preview": serialized.chars().take(2_000).collect::<String>(),
        }),
        true,
        size,
    )
}

/// Reads the guest's `result.json` envelope with a hard byte ceiling (audit
/// #121). `Ok(None)` covers "no parseable envelope" (missing file, unreadable,
/// or invalid JSON) — the caller's missing-envelope path. `Err` is the one
/// case that must not be collapsed into that path: the file exists but
/// overflowed the cap, which gets its own `ResultTooLarge` verdict.
fn read_result_envelope(path: &std::path::Path) -> Result<Option<Value>, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let mut buf = Vec::new();
    if file
        .take(RESULT_FILE_MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .is_err()
    {
        return Ok(None);
    }
    if buf.len() as u64 > RESULT_FILE_MAX_BYTES {
        return Err(format!(
            "result.json exceeded the {} byte host read cap",
            RESULT_FILE_MAX_BYTES
        ));
    }
    Ok(serde_json::from_slice::<Value>(&buf).ok())
}

/// Runs `code` in the sandboxed CPython guest.
///
/// `guest_path` must already be resolved (see `guest::ensure_python_guest`);
/// this function does no network I/O.
pub fn run_python(
    guest_path: &std::path::Path,
    code: &str,
    variables: &Map<String, Value>,
    timeout: Duration,
    workspace: Option<&std::path::Path>,
) -> anyhow::Result<Value> {
    if code.len() > MAX_CODE_LENGTH_BYTES {
        return Ok(json!({
            "status": "error",
            "result": null,
            "stdout": "",
            "execution_time_ms": 0,
            "error": {
                "error_type": "CodeTooLarge",
                "message": format!(
                    "Script length exceeds maximum cap of {MAX_CODE_LENGTH_BYTES} bytes."
                ),
            },
            "result_truncated": false,
            "result_size_bytes": 0,
        }));
    }

    let scratch = tempdir()?;
    // Inputs read-only, outputs write-only-ish: the guest gets exactly the
    // two capabilities it needs and nothing more.
    let in_dir = scratch.path().join("in");
    let out_dir = scratch.path().join("out");
    std::fs::create_dir_all(&in_dir)?;
    std::fs::create_dir_all(&out_dir)?;
    std::fs::write(in_dir.join("code.py"), code)?;
    std::fs::write(in_dir.join("vars.json"), serde_json::to_string(variables)?)?;

    let module = sandbox::load_module_cached(guest_path, &guest::module_cache_dir())?;

    let mut mounts = vec![
        Mount::read_only(&in_dir, "/kitty"),
        Mount::writable(&out_dir, "/kitty-out"),
    ];
    let mut env: Vec<(String, String)> = Vec::new();

    // Optional pure-Python package tree (e.g. networkx). Read-only, and only
    // when the user actually created it — see `guest::site_packages_dir`.
    let site_packages = guest::site_packages_dir();
    if site_packages.is_dir() {
        mounts.push(Mount::read_only(&site_packages, "/site-packages"));
        env.push(("PYTHONPATH".to_string(), "/site-packages".to_string()));
    }

    // The caller's working directory, writable, so scripts can actually read
    // and produce files. Everything outside it stays unreachable.
    if let Some(workspace) = workspace {
        mounts.push(Mount::writable(workspace, "/work"));
    }

    let request = RunRequest {
        args: vec!["python".into(), "-c".into(), HARNESS.into()],
        env,
        mounts,
        timeout,
        ..Default::default()
    };

    let output = sandbox::run_module(&module, &request)?;
    let stdout = output.stdout.contents();
    let stderr = output.stderr.contents();
    let elapsed_ms = output.duration.as_millis() as u64;

    let envelope = match read_result_envelope(&out_dir.join("result.json")) {
        Ok(env) => env,
        Err(too_large) => {
            // An oversized envelope is not "the guest died before
            // reporting" — it reported too much. Distinct error type so the
            // caller can tell the difference (audit #121).
            return Ok(json!({
                "status": "error",
                "result": null,
                "stdout": stdout,
                "stderr": stderr,
                "execution_time_ms": elapsed_ms,
                "error": {"error_type": "ResultTooLarge", "message": too_large},
                "result_truncated": false,
                "result_size_bytes": 0,
                "outcome": output.outcome.label(),
            }));
        }
    };

    // A missing envelope means the guest died before the harness could report
    // — timeout, trap, or a hard `os._exit`. Those are exactly the cases the
    // Python original could not represent at all (its worker just vanished),
    // so they get first-class, distinguishable error types here.
    let Some(envelope) = envelope else {
        let (error_type, message) = match &output.outcome {
            Outcome::TimedOut => (
                "TimeoutError",
                format!("Execution exceeded the {}s time limit.", timeout.as_secs()),
            ),
            Outcome::OutOfFuel => (
                "FuelExhausted",
                "Execution exceeded its deterministic instruction budget.".to_string(),
            ),
            Outcome::Trapped { message } => ("WasmTrap", message.clone()),
            Outcome::Exited { code } => (
                "NoResult",
                format!(
                    "The interpreter exited with code {code} without reporting a result."
                ),
            ),
        };
        return Ok(json!({
            "status": "error",
            "result": null,
            "stdout": stdout,
            "stderr": stderr,
            "execution_time_ms": elapsed_ms,
            "error": {"error_type": error_type, "message": message},
            "result_truncated": false,
            "result_size_bytes": 0,
            "outcome": output.outcome.label(),
        }));
    };

    let status = envelope
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("error")
        .to_string();
    let raw_result = envelope.get("result").cloned().unwrap_or(Value::Null);
    // The guest already capped and measured this. Re-running the host cap is
    // deliberate defense in depth (the guest could have miscounted, or a
    // future guest might not implement it), but the guest's own byte count is
    // the more accurate figure when it reported one, and its truncation flag
    // must not be lost just because the replacement value is small.
    let guest_truncated = envelope
        .get("result_truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let guest_size = envelope
        .get("result_size_bytes")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let (result, host_truncated, host_size) = truncate_result(raw_result);
    let result_truncated = guest_truncated || host_truncated;
    let result_size_bytes = guest_size.unwrap_or(host_size);

    let mut out = Map::new();
    out.insert("status".into(), json!(status));
    out.insert("result".into(), result);
    out.insert("stdout".into(), json!(stdout));
    if !stderr.is_empty() {
        out.insert("stderr".into(), json!(stderr));
    }
    out.insert("execution_time_ms".into(), json!(elapsed_ms));
    out.insert(
        "error".into(),
        envelope.get("error").cloned().unwrap_or(Value::Null),
    );
    out.insert("result_truncated".into(), json!(result_truncated));
    out.insert("result_size_bytes".into(), json!(result_size_bytes));
    if output.stdout.truncated() {
        out.insert("stdout_truncated".into(), json!(true));
        out.insert("stdout_total_bytes".into(), json!(output.stdout.total_bytes()));
    }

    Ok(Value::Object(out))
}

/// `tempfile::tempdir` lives in dev-dependencies only; this is the one place
/// production code needs a scratch directory, and it needs exactly one
/// behavior (unique dir, removed on drop), so it's cheaper to implement than
/// to promote a dependency.
fn tempdir() -> anyhow::Result<ScratchDir> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "kitty-wasm-{}-{nanos:x}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path)?;
    Ok(ScratchDir { path })
}

pub struct ScratchDir {
    path: std::path::PathBuf,
}

impl ScratchDir {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_result_cap_agrees_with_the_rust_constant() {
        // The harness is Python source, so `MAX_RESULT_BYTES` has to be
        // written out as a literal there. If someone changes the Rust
        // constant without changing the harness, the guest would cap at a
        // different size than the host documents — this is the tripwire.
        assert!(
            HARNESS.contains(&format!("_KITTY_MAX_RESULT_BYTES = {MAX_RESULT_BYTES}")),
            "harness cap literal is out of sync with MAX_RESULT_BYTES"
        );
    }

    #[test]
    fn harness_writes_to_the_documented_result_path() {
        // The host reads `out_dir/result.json`; the guest writes
        // `/kitty-out/result.json`. Those are the same file only because the
        // mount point agrees, so pin the string the harness actually uses.
        assert!(HARNESS.contains(RESULT_PATH));
    }

    #[test]
    fn harness_never_writes_the_envelope_to_a_capped_stream() {
        // The bug this replaced: routing the envelope through stderr meant a
        // large result was truncated mid-JSON by the ring buffer and became
        // unparseable. stdout/stderr must carry only the user's own output.
        assert!(
            !HARNESS.contains("stderr.write"),
            "envelope must not go through the byte-capped stderr stream"
        );
        assert!(!HARNESS.contains("stdout.write"));
    }

    #[test]
    fn small_results_pass_through_untruncated() {
        let (value, truncated, size) = truncate_result(json!({"a": [1, 2, 3]}));
        assert_eq!(value, json!({"a": [1, 2, 3]}));
        assert!(!truncated);
        assert!(size > 0 && size < MAX_RESULT_BYTES);
    }

    #[test]
    fn oversized_results_are_replaced_with_valid_json_not_sliced() {
        let big: Vec<u64> = (0..200_000).collect();
        let (value, truncated, size) = truncate_result(json!(big));
        assert!(truncated);
        assert!(size > MAX_RESULT_BYTES);
        // The replacement must itself be well-formed and self-describing.
        assert_eq!(value["truncated"], json!(true));
        assert!(value["reason"].as_str().unwrap().contains("cap"));
        assert!(!value["preview"].as_str().unwrap().is_empty());
        serde_json::to_string(&value).expect("replacement must serialize");
    }

    #[test]
    fn oversized_code_is_rejected_before_the_sandbox_is_even_started() {
        let huge = "x".repeat(MAX_CODE_LENGTH_BYTES + 1);
        // A bogus guest path proves no sandbox work happened: the size check
        // returns before the guest is ever loaded.
        let out = run_python(
            std::path::Path::new("/nonexistent/guest.wasm"),
            &huge,
            &Map::new(),
            Duration::from_secs(5),
            None,
        )
        .expect("size rejection is a value, not an Err");
        assert_eq!(out["status"], "error");
        assert_eq!(out["error"]["error_type"], "CodeTooLarge");
    }

    #[test]
    fn scratch_dir_is_unique_and_cleaned_up_on_drop() {
        let path = {
            let a = tempdir().unwrap();
            let b = tempdir().unwrap();
            assert_ne!(a.path(), b.path());
            assert!(a.path().is_dir());
            a.path().to_path_buf()
        };
        assert!(!path.exists(), "scratch dir outlived its guard");
    }

    #[test]
    fn result_envelope_read_is_byte_capped() {
        // Audit #121: the host used to `read_to_string` the guest-written
        // file with no cap — a guest filling `/kitty-out` would OOM the host.
        let dir = tempdir().unwrap();
        let small = dir.path().join("small.json");
        std::fs::write(&small, r#"{"status":"success","result":1}"#).unwrap();
        let v = read_result_envelope(&small).unwrap().expect("parses");
        assert_eq!(v["result"], json!(1));

        let big = dir.path().join("big.json");
        std::fs::write(&big, vec![b'x'; (RESULT_FILE_MAX_BYTES + 1) as usize]).unwrap();
        let err = read_result_envelope(&big).unwrap_err();
        assert!(err.contains("cap"), "{err}");

        // Missing/corrupt files stay on the ordinary `None` path.
        assert_eq!(read_result_envelope(&dir.path().join("missing.json")).unwrap(), None);
        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"not json").unwrap();
        assert_eq!(read_result_envelope(&corrupt).unwrap(), None);
    }
}
