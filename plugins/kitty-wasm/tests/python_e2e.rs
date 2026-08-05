//! End-to-end tests against the real CPython WASI guest.
//!
//! All `#[ignore]`d so `cargo test` stays hermetic and offline: the guest is a
//! 26 MB download, not a committed fixture. Run them with the guest available:
//!
//! ```text
//! # one-time, or point KITTY_WASM_PYTHON at an existing copy
//! cargo test --test python_e2e -- --ignored --nocapture
//! ```
//!
//! The unit tests in `sandbox.rs` prove the sandbox mechanics against
//! hand-written WAT. These prove the thing that actually matters to a user:
//! that a real interpreter runs real code under those mechanics, and that the
//! capability boundary genuinely holds against a language that will happily
//! try to open sockets and read `/etc/passwd`.

use std::time::Duration;

use serde_json::{Map, Value};

fn guest() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("KITTY_WASM_PYTHON") {
        return std::path::PathBuf::from(explicit);
    }
    kitty_wasm::guest::guests_dir().join(kitty_wasm::guest::PYTHON_GUEST_FILENAME)
}

fn run(code: &str) -> Value {
    run_with(code, &Map::new(), None, Duration::from_secs(60))
}

fn run_with(
    code: &str,
    vars: &Map<String, Value>,
    workspace: Option<&std::path::Path>,
    timeout: Duration,
) -> Value {
    let path = guest();
    assert!(
        path.is_file(),
        "guest not found at {}. Install it first (wasm_guest_status install=true) \
         or set KITTY_WASM_PYTHON.",
        path.display()
    );
    kitty_wasm::python::run_python(&path, code, vars, timeout, workspace)
        .expect("host-side setup should succeed")
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn runs_python_and_returns_a_structured_result() {
    let out = run("result = sum(range(101))");
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(out["result"], 5050);
    assert_eq!(out["error"], Value::Null);
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn captures_stdout_separately_from_the_result() {
    let out = run("print('hello from the sandbox')\nresult = {'ok': True}");
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(out["stdout"], "hello from the sandbox\n");
    assert_eq!(out["result"]["ok"], true);
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn the_standard_library_is_available() {
    // The stdlib set `wasm_math_mcp.py`'s sandbox exposed, which is the
    // compatibility bar this replacement has to clear.
    let out = run(
        r#"
import math, cmath, statistics, decimal, fractions, datetime, calendar
import json, re, itertools, heapq, collections, random, textwrap, unicodedata
result = {
    "sqrt2": round(math.sqrt(2), 6),
    "mean": statistics.mean([1, 2, 3, 4]),
    "stdev": round(statistics.stdev([1, 2, 3, 4]), 6),
    "exact": str(decimal.Decimal("0.1") + decimal.Decimal("0.2")),
    "frac": str(fractions.Fraction(3, 6)),
    "combos": len(list(itertools.combinations(range(6), 3))),
    "date": datetime.date(2025, 3, 4).isoformat(),
    "re": re.findall(r"\d+", "a1b22c333"),
    "complex": str(cmath.sqrt(-1)),
}
"#,
    );
    assert_eq!(out["status"], "success", "{out}");
    let r = &out["result"];
    #[allow(clippy::approx_constant)] // this is Python's output, not a Rust constant
    let expected_sqrt2 = 1.414214;
    assert_eq!(r["sqrt2"], expected_sqrt2);
    assert_eq!(r["mean"], 2.5);
    assert_eq!(r["exact"], "0.3");
    assert_eq!(r["frac"], "1/2");
    assert_eq!(r["combos"], 20);
    assert_eq!(r["date"], "2025-03-04");
    assert_eq!(r["re"], serde_json::json!(["1", "22", "333"]));
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn injected_variables_are_visible_as_globals() {
    let mut vars = Map::new();
    vars.insert("xs".into(), serde_json::json!([5, 3, 8, 1]));
    vars.insert("label".into(), serde_json::json!("totals"));
    let out = run_with(
        "result = {'label': label, 'sorted': sorted(xs), 'total': sum(xs)}",
        &vars,
        None,
        Duration::from_secs(60),
    );
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(out["result"]["label"], "totals");
    assert_eq!(out["result"]["total"], 17);
    assert_eq!(out["result"]["sorted"], serde_json::json!([1, 3, 5, 8]));
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn a_python_exception_reports_type_message_and_line_number() {
    let out = run("x = 1\ny = 2\nresult = x / 0\n");
    assert_eq!(out["status"], "error", "{out}");
    assert_eq!(out["error"]["error_type"], "ZeroDivisionError");
    assert_eq!(out["error"]["line"], 3, "should point at the failing line");
    assert!(out["error"]["source_line"].as_str().unwrap().contains("x / 0"));
    assert_eq!(out["result"], Value::Null);
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn a_syntax_error_is_reported_not_swallowed() {
    let out = run("def broken(:\n    pass\n");
    assert_eq!(out["status"], "error", "{out}");
    assert_eq!(out["error"]["error_type"], "SyntaxError");
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn stdout_written_before_a_crash_is_still_returned() {
    let out = run("print('progress so far')\nraise ValueError('boom')");
    assert_eq!(out["status"], "error");
    assert_eq!(out["error"]["error_type"], "ValueError");
    assert!(out["stdout"].as_str().unwrap().contains("progress so far"));
}

// --- capability boundary -------------------------------------------------

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn the_sandbox_has_no_network_access() {
    let out = run(
        r#"
try:
    import socket
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(("1.1.1.1", 80))
    result = {"reached_network": True}
except BaseException as e:
    result = {"reached_network": False, "why": type(e).__name__}
"#,
    );
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(
        out["result"]["reached_network"], false,
        "the sandbox reached the network: {out}"
    );
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn the_sandbox_cannot_read_the_host_filesystem() {
    // Nothing is mounted here, so *no* host path should be reachable —
    // including the ones a real attacker would reach for first.
    let out = run(
        r#"
import os
probes = ["C:/Windows/System32/drivers/etc/hosts", "/etc/passwd", "C:/", "/"]
reachable = []
for p in probes:
    try:
        if os.path.isdir(p):
            os.listdir(p)
        else:
            open(p, "rb").read(16)
        reachable.append(p)
    except BaseException:
        pass
result = {"reachable": reachable}
"#,
    );
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(
        out["result"]["reachable"],
        serde_json::json!([]),
        "host filesystem was reachable from the sandbox: {out}"
    );
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn a_mounted_workspace_is_readable_and_writable_but_nothing_above_it_is() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("input.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let out = run_with(
        r#"
import os
with open("/work/input.txt") as f:
    lines = [l.strip() for l in f if l.strip()]
with open("/work/output.txt", "w") as f:
    f.write(",".join(reversed(lines)))

escaped = False
try:
    os.listdir("/work/../..")
    escaped = True
except BaseException:
    pass

result = {"lines": lines, "escaped": escaped}
"#,
        &Map::new(),
        Some(dir.path()),
        Duration::from_secs(60),
    );
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(out["result"]["lines"], serde_json::json!(["alpha", "beta", "gamma"]));
    assert_eq!(out["result"]["escaped"], false, "escaped the workspace mount");

    // The write really landed on the host, in the workspace only.
    let written = std::fs::read_to_string(dir.path().join("output.txt")).unwrap();
    assert_eq!(written, "gamma,beta,alpha");
}

// --- resource limits ------------------------------------------------------

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn an_infinite_loop_hits_the_timeout_and_reports_it_distinctly() {
    let started = std::time::Instant::now();
    let out = run_with(
        "while True:\n    pass\n",
        &Map::new(),
        None,
        Duration::from_secs(3),
    );
    let elapsed = started.elapsed();

    assert_eq!(out["status"], "error", "{out}");
    assert_eq!(out["error"]["error_type"], "TimeoutError");
    assert_eq!(out["outcome"], "timed_out");
    assert!(
        elapsed < Duration::from_secs(45),
        "timeout did not actually stop the guest (took {elapsed:?})"
    );
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn runaway_output_is_truncated_rather_than_trapping_the_guest() {
    let out = run("for i in range(200000):\n    print('line', i)\nresult = 'finished'");
    assert_eq!(out["status"], "success", "chatty script must still finish: {out}");
    assert_eq!(out["result"], "finished");
    assert_eq!(out["stdout_truncated"], true);
    assert!(out["stdout"].as_str().unwrap().contains("bytes omitted"));
    // Bounded regardless of how much the script actually printed.
    assert!(out["stdout"].as_str().unwrap().len() < 60_000);
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn an_oversized_result_is_replaced_with_well_formed_json() {
    let out = run("result = list(range(400000))");
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(out["result_truncated"], true);
    assert_eq!(out["result"]["truncated"], true);
    assert!(out["result_size_bytes"].as_u64().unwrap() > 256_000);
}

#[test]
#[ignore = "requires the 26 MB CPython guest"]
fn module_caching_makes_the_second_run_dramatically_faster() {
    // First call may pay the ~20s cold compile; the cache must make the
    // next one fast. This is the difference between the tool being usable
    // and unusable, so it's worth pinning.
    run("result = 1");
    let started = std::time::Instant::now();
    let out = run("result = 2");
    let warm = started.elapsed();
    assert_eq!(out["result"], 2);
    assert!(
        warm < Duration::from_secs(10),
        "warm run took {warm:?}; the module cache is not working"
    );
}
