import ast
import atexit
import calendar
import cmath
import collections
from collections import deque
import contextlib
import datetime
import decimal
import fractions
import heapq
import io
import itertools
import json
import math
import multiprocessing
from multiprocessing.queues import Empty
import re
import statistics
import threading
import time
import traceback
from typing import Annotated, Any, Dict, List, Optional, Tuple

from pydantic import Field

import networkx as nx
from mcp.server.fastmcp import FastMCP

# Initialize FastMCP Server
mcp = FastMCP("Lightweight Graph & Math MCP Server")

# Maximum input payload size (50 KB cap to prevent RAM exhaustion)
MAX_CODE_LENGTH_BYTES = 50_000

# F1: Maximum result JSON size before truncation (256 KB)
MAX_RESULT_BYTES = 256_000

# F4: Human-readable list of available modules/names in the sandbox
AVAILABLE_NAMES = (
    "math, cmath, decimal, fractions, statistics, networkx (as 'nx'), "
    "collections, itertools, heapq, datetime, calendar, json, re, "
    "and standard builtins (list, dict, set, str, int, float, bool, len, "
    "sum, max, min, sorted, enumerate, zip, map, range, abs, round, "
    "pow, divmod, reversed, format, type, isinstance, "
    "ValueError, TypeError)"
)

# ---------------------------------------------------------------------------
# Module-Level Safe Scope (Zero-Heavy-Dependency Stack)
# ---------------------------------------------------------------------------
SAFE_GLOBALS: Dict[str, Any] = {
    "__builtins__": {
        "abs": abs, "all": all, "any": any, "bool": bool, "complex": complex,
        "dict": dict, "divmod": divmod, "enumerate": enumerate, "float": float,
        "format": format, "frozenset": frozenset, "int": int, "len": len,
        "list": list, "map": map, "max": max, "min": min, "pow": pow,
        "print": print, "range": range, "repr": repr, "reversed": reversed,
        "round": round, "set": set, "slice": slice, "sorted": sorted,
        "str": str, "sum": sum, "tuple": tuple, "zip": zip,
        "isinstance": isinstance, "type": type, "ValueError": ValueError, "TypeError": TypeError,
    },
    # Math & Precision
    "math": math,
    "cmath": cmath,
    "decimal": decimal,
    "fractions": fractions,
    "statistics": statistics,
    # Graphs & Algorithms
    "networkx": nx,
    "nx": nx,
    "collections": collections,
    "itertools": itertools,
    "heapq": heapq,
    # Utilities & Parsing
    "datetime": datetime,
    "calendar": calendar,
    "json": json,
    "re": re,
}


def _elapsed_ms(start_time: float) -> float:
    """Helper to calculate elapsed execution time in milliseconds."""
    return round((time.perf_counter() - start_time) * 1000, 2)


# ---------------------------------------------------------------------------
# Smart Memory-Bounded Stdout Stream Writer
# ---------------------------------------------------------------------------
class SmartStdoutBuffer(io.TextIOBase):
    """Memory-bounded stdout stream writer using a Head/Tail ring buffer strategy.

    Guarantees stdout in RAM never exceeds ~45 KB regardless of script output size.
    """

    def __init__(self, head_limit_bytes: int = 20_000, tail_limit_bytes: int = 25_000):
        super().__init__()
        self.head_limit = head_limit_bytes
        self.tail_limit = tail_limit_bytes

        self.head_buf: List[str] = []
        self.head_bytes = 0

        self.tail_deque: deque = deque()
        self.tail_bytes = 0

        self.total_bytes = 0
        self.total_lines = 0
        self.is_truncated = False

    def write(self, s: str) -> int:
        if not s:
            return 0

        n_bytes = len(s) if s.isascii() else len(s.encode("utf-8"))
        self.total_bytes += n_bytes
        self.total_lines += s.count("\n")

        if self.head_bytes < self.head_limit:
            needed = self.head_limit - self.head_bytes
            if n_bytes <= needed:
                self.head_buf.append(s)
                self.head_bytes += n_bytes
            else:
                head_part = s[:needed]
                tail_part = s[needed:]
                head_part_bytes = len(head_part) if head_part.isascii() else len(head_part.encode("utf-8"))
                self.head_buf.append(head_part)
                self.head_bytes += head_part_bytes
                self._push_tail(tail_part)
                self.is_truncated = True
        else:
            self.is_truncated = True
            self._push_tail(s)

        return len(s)

    def _push_tail(self, s: str) -> None:
        n_bytes = len(s) if s.isascii() else len(s.encode("utf-8"))
        self.tail_deque.append((s, n_bytes))
        self.tail_bytes += n_bytes

        while self.tail_bytes > self.tail_limit and self.tail_deque:
            _, popped_bytes = self.tail_deque.popleft()
            self.tail_bytes -= popped_bytes

    def getvalue(self) -> str:
        if not self.is_truncated:
            return "".join(self.head_buf).strip()

        head_str = "".join(self.head_buf)
        tail_str = "".join(item[0] for item in self.tail_deque)
        dropped_bytes = self.total_bytes - (self.head_bytes + self.tail_bytes)

        marker = (
            f"\n\n--- [STDOUT TRUNCATED: Skipped {dropped_bytes:,} bytes (~{self.total_lines:,} lines total) "
            f"to prevent memory overflow. Displaying initial {self.head_bytes // 1000}KB and final {self.tail_bytes // 1000}KB below.] ---\n\n"
        )
        return (head_str + marker + tail_str).strip()


# ---------------------------------------------------------------------------
# Helper & AST Transformation Functions
# ---------------------------------------------------------------------------
def _format_code_with_line_numbers(code: str) -> str:
    """Formats Python code with explicit line numbers for LLM error context."""
    if not code:
        return "1: <empty code>"
    lines = code.splitlines()
    width = len(str(len(lines))) if lines else 1
    return "\n".join(f"{i + 1:>{width}}: {line}" for i, line in enumerate(lines))


def analyze_transform_ast(code: str) -> Tuple[bool, Optional[str], Optional[ast.AST]]:
    """Single-pass AST validator and transformer with input payload capping."""
    if len(code.encode("utf-8")) > MAX_CODE_LENGTH_BYTES:
        return False, f"Security Restriction: Script length exceeds maximum cap of {MAX_CODE_LENGTH_BYTES:,} bytes.", None

    try:
        tree = ast.parse(code, mode="exec")
    except SyntaxError as e:
        error_msg = f"SyntaxError on line {e.lineno}, col {e.offset}: {e.msg}\n  Code: {e.text.strip() if e.text else ''}"
        return False, error_msg, None

    banned_modules = {
        "os", "sys", "subprocess", "shutil", "socket", "ctypes", "pathlib", "importlib", "pickle"
    }
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name.split(".")[0] in banned_modules:
                    return False, f"Security Restriction: Import of '{alias.name}' is prohibited.", None
        elif isinstance(node, ast.ImportFrom):
            if node.module and node.module.split(".")[0] in banned_modules:
                return False, f"Security Restriction: Import from '{node.module}' is prohibited.", None

    # Auto-return last expression in multi-line scripts
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        last_expr = tree.body[-1].value
        tree.body[-1] = ast.Assign(
            targets=[ast.Name(id="_last_result", ctx=ast.Store())],
            value=last_expr,
        )
        ast.fix_missing_locations(tree)

    return True, None, tree


# ---------------------------------------------------------------------------
# F2: Deterministic ordering + path-based cycle detection
# ---------------------------------------------------------------------------
def _sanitize_result(obj: Any, _seen: Optional[set] = None) -> Any:
    """Recursively converts complex data structures to JSON-serializable primitives.

    Uses a path-based cycle detector to avoid false positives on shared-but-not-circular
    container objects. All dict keys are sorted for deterministic output. All sets are
    converted to sorted lists.
    """
    if obj is None or isinstance(obj, (int, float, str, bool)):
        return obj

    if _seen is None:
        _seen = set()

    is_container = isinstance(obj, (dict, list, tuple, set, frozenset, nx.Graph))
    if is_container:
        obj_id = id(obj)
        if obj_id in _seen:
            return "<circular_reference>"
        _seen.add(obj_id)

    try:
        return _sanitize_result_impl(obj, _seen)
    finally:
        if is_container:
            _seen.discard(id(obj))


def _sorted_keys(keys) -> list:
    """Sort keys deterministically: strings and numbers first, then by type name."""
    try:
        return sorted(keys, key=lambda k: (type(k).__name__, str(k)))
    except TypeError:
        return list(keys)


def _sanitize_result_impl(obj: Any, _seen: set) -> Any:
    """Internal dispatch for _sanitize_result after cycle check."""
    if isinstance(obj, nx.Graph):
        nodes: List[Dict[str, Any]] = []
        for n, d in sorted(obj.nodes(data=True), key=lambda x: str(x[0])):
            sanitized_d = _sanitize_result(d, _seen)
            node_dict: Dict[str, Any] = {"id": n}
            if isinstance(sanitized_d, dict):
                node_dict.update(sanitized_d)
            else:
                node_dict["data"] = sanitized_d
            nodes.append(node_dict)

        edges: List[Dict[str, Any]] = []
        if obj.is_multigraph():
            for u, v, k, d in sorted(obj.edges(keys=True, data=True),
                                     key=lambda x: (str(x[0]), str(x[1]), str(x[2]))):
                sanitized_d = _sanitize_result(d, _seen)
                edge_dict: Dict[str, Any] = {"source": u, "target": v, "key": k}
                if isinstance(sanitized_d, dict):
                    edge_dict.update(sanitized_d)
                else:
                    edge_dict["data"] = sanitized_d
                edges.append(edge_dict)
        else:
            for u, v, d in sorted(obj.edges(data=True),
                                  key=lambda x: (str(x[0]), str(x[1]))):
                sanitized_d = _sanitize_result(d, _seen)
                edge_dict: Dict[str, Any] = {"source": u, "target": v}
                if isinstance(sanitized_d, dict):
                    edge_dict.update(sanitized_d)
                else:
                    edge_dict["data"] = sanitized_d
                edges.append(edge_dict)

        return {
            "directed": obj.is_directed(),
            "multigraph": obj.is_multigraph(),
            "nodes": nodes,
            "edges": edges,
            "graph_metadata": _sanitize_result(dict(obj.graph), _seen),
        }

    if isinstance(obj, (decimal.Decimal, fractions.Fraction)):
        return str(obj)

    if isinstance(obj, bytes):
        return obj.decode("utf-8", errors="replace")

    # F2: Sorted set serialization
    if isinstance(obj, (set, frozenset)):
        try:
            return sorted(_sanitize_result(item, _seen) for item in obj)
        except TypeError:
            return [_sanitize_result(item, _seen) for item in obj]

    if isinstance(obj, complex):
        return {"real": obj.real, "imag": obj.imag}

    if isinstance(obj, (datetime.datetime, datetime.date, datetime.time)):
        return obj.isoformat()

    # F2: Sorted dict key serialization
    if isinstance(obj, dict):
        return {str(k): _sanitize_result(v, _seen)
                for k in _sorted_keys(obj.keys())}

    if isinstance(obj, (list, tuple)):
        return [_sanitize_result(item, _seen) for item in obj]

    return obj


def format_actionable_error(exc: Exception, code: str) -> Dict[str, Any]:
    """Extracts line numbers, code context, line-numbered script, and actionable hints."""
    tb = traceback.extract_tb(exc.__traceback__)
    code_lines = code.splitlines()

    line_no = None
    line_content = None

    if isinstance(exc, SyntaxError):
        line_no = exc.lineno
        line_content = exc.text.strip() if exc.text else ""
    elif tb:
        for frame in reversed(tb):
            if frame.filename in ("<string>", "<ast>", "<eval>"):
                line_no = frame.lineno
                if line_no and 1 <= line_no <= len(code_lines):
                    line_content = code_lines[line_no - 1].strip()
                break

    error_info: Dict[str, Any] = {
        "error_type": type(exc).__name__,
        "message": str(exc),
        "line_number": line_no,
        "line_code": line_content,
        "code_with_line_numbers": _format_code_with_line_numbers(code),
    }

    if isinstance(exc, NameError):
        exc_str = str(exc)
        # Precomputed rather than inlined: a backslash-escaped quote inside
        # an f-string expression part is a SyntaxError before Python 3.12
        # (PEP 701), and this plugin is frozen against 3.11.
        undefined_name = exc_str.split("'")[1] if "'" in exc_str else "unknown"
        error_info["hint"] = (
            f"Name '{undefined_name}' is not defined. "
            f"Available modules and names: {AVAILABLE_NAMES}"
        )
    elif isinstance(exc, TypeError):
        error_info["hint"] = "Check type compatibility. Convert strings or complex types explicitly."
    elif isinstance(exc, ZeroDivisionError):
        error_info["hint"] = "Divisor evaluated to zero. Add a conditional check or fallback."
    elif isinstance(exc, KeyError):
        error_info["hint"] = "Key missing in dictionary or graph node. Verify keys or node IDs."
    elif isinstance(exc, IndexError):
        error_info["hint"] = "Index out of range. Check list or deque dimensions prior to indexing."
    elif isinstance(exc, SyntaxError):
        error_info["hint"] = "Fix syntax error (check missing colons, unbalanced brackets, or indentation)."
    elif isinstance(exc, nx.NetworkXError):
        error_info["hint"] = "Verify NetworkX graph constraints (e.g., check for cycles in DAGs, valid paths, or existing nodes)."
    else:
        error_info["hint"] = "Review the line-numbered code and error message to correct execution logic."

    return error_info


# ---------------------------------------------------------------------------
# Thread-per-request execution with timeout & worker recycling
# ---------------------------------------------------------------------------
def _execute_single_request(
    code: str,
    variables: Dict[str, Any],
    heartbeat_interval: float,
) -> Dict[str, Any]:
    """Execute one code request. Called inside a dedicated daemon thread.

    F6: Heartbeat — periodically writes a progress indicator to stdout so the caller
    knows execution is still active during long-running operations.
    """
    start_time = time.perf_counter()

    is_valid, ast_error, tree = analyze_transform_ast(code)
    if not is_valid:
        return {
            "status": "error",
            "result": None,
            "stdout": "",
            "execution_time_ms": _elapsed_ms(start_time),
            "error": {
                "error_type": "ASTValidationError",
                "message": ast_error,
                "code_with_line_numbers": _format_code_with_line_numbers(code),
                "hint": "Fix syntax or remove restricted module imports before re-submitting.",
            },
        }

    execution_scope = SAFE_GLOBALS.copy()
    if variables:
        execution_scope.update(variables)

    buffer = SmartStdoutBuffer(head_limit_bytes=20_000, tail_limit_bytes=25_000)

    # F6: Heartbeat thread — writes a marker to the buffer at a fixed interval
    heartbeat_active = threading.Event()
    heartbeat_thread = threading.Thread(
        target=_heartbeat_loop,
        args=(buffer, heartbeat_active, heartbeat_interval),
        daemon=True,
    )
    eval_result = None

    try:
        heartbeat_active.set()
        heartbeat_thread.start()

        with contextlib.redirect_stdout(buffer):
            compiled_code = compile(tree, filename="<ast>", mode="exec")
            exec(compiled_code, execution_scope)

            if "result" in execution_scope:
                eval_result = execution_scope["result"]
            elif "_last_result" in execution_scope:
                eval_result = execution_scope["_last_result"]

        heartbeat_active.clear()
        heartbeat_thread.join(timeout=0.5)

        sanitized_result = _sanitize_result(eval_result)

        # F1: Result size cap with truncation
        result_json = json.dumps(sanitized_result, default=str)
        result_bytes = len(result_json.encode("utf-8"))
        result_truncated = False

        if result_bytes > MAX_RESULT_BYTES:
            result_truncated = True
            # Keep the JSON prefix intact — truncate the inner content
            truncated_result = _truncate_json(result_json, MAX_RESULT_BYTES)
            sanitized_result = truncated_result

        return {
            "status": "success",
            "result": sanitized_result,
            "stdout": buffer.getvalue(),
            "execution_time_ms": _elapsed_ms(start_time),
            "error": None,
            "result_truncated": result_truncated,
            "result_size_bytes": result_bytes,
        }

    except Exception as exc:
        heartbeat_active.clear()
        heartbeat_thread.join(timeout=0.5)
        return {
            "status": "error",
            "result": None,
            "stdout": buffer.getvalue(),
            "execution_time_ms": _elapsed_ms(start_time),
            "error": format_actionable_error(exc, code),
        }


def _heartbeat_loop(
    buffer: SmartStdoutBuffer,
    active: threading.Event,
    interval: float,
) -> None:
    """F6: Periodically write a heartbeat marker to stdout while execution is active."""
    while active.wait(timeout=interval):
        buffer.write("\n[HEARTBEAT]\n")


def _truncate_json(json_str: str, max_bytes: int) -> Any:
    """F1: Truncate a JSON string to stay within max_bytes while preserving structure.

    Returns a dict containing the truncated result, metadata about the truncation,
    and an explanation for the caller.
    """
    try:
        full_obj = json.loads(json_str)
    except json.JSONDecodeError:
        full_obj = None

    truncated = {
        "_truncated": True,
        "_message": (
            f"Result was truncated from {len(json_str):,} bytes to {max_bytes:,} bytes "
            f"to prevent context overflow. The full result structure is preserved below "
            f"with nested collections capped."
        ),
        "_original_size_bytes": len(json_str),
        "_data": _deep_truncate(full_obj, max_bytes - 500),
    }
    return truncated


def _deep_truncate(obj: Any, remaining_bytes: int, _depth: int = 0) -> Any:
    """Recursively cap list/dict sizes until the result fits within remaining_bytes."""
    if _depth > 20:
        return "<max nesting depth>"

    if isinstance(obj, (int, float, str, bool, type(None))):
        return obj

    if isinstance(obj, list):
        if not obj:
            return obj
        for limit in [min(len(obj), 1000), 500, 250, 100, 50, 10, 5, 1]:
            truncated = [_deep_truncate(item, remaining_bytes // limit, _depth + 1) for item in obj[:limit]]
            test = json.dumps(truncated, default=str)
            if len(test.encode("utf-8")) <= remaining_bytes:
                if limit < len(obj):
                    truncated.append(f"<... {len(obj) - limit:,} more items elided >")
                return truncated
        return obj[:1]

    if isinstance(obj, dict):
        if not obj:
            return obj
        items = list(obj.items())
        for limit in [min(len(items), 200), 100, 50, 25, 10, 5, 1]:
            truncated = {str(k): _deep_truncate(v, remaining_bytes // limit, _depth + 1)
                         for k, v in items[:limit]}
            test = json.dumps(truncated, default=str)
            if len(test.encode("utf-8")) <= remaining_bytes:
                if limit < len(items):
                    truncated[f"<... {len(items) - limit:,} more keys elided >"] = None
                return truncated
        return {str(items[0][0]): items[0][1]}

    return obj


# ---------------------------------------------------------------------------
# Persistent Worker & Execution Engine
# ---------------------------------------------------------------------------
def _worker_loop(
    request_queue: multiprocessing.Queue,
    response_queue: multiprocessing.Queue,
) -> None:
    """Persistent worker loop. Each request runs in its own daemon thread.

    Timeout enforcement: If a CPU runaway thread exceeds the timeout, the worker
    reports TimeoutError and breaks out of the loop. This causes the worker process
    to terminate cleanly so PersistentSandboxManager can spawn a fresh worker process
    free of zombie CPU threads.
    """
    while True:
        try:
            req = request_queue.get()
            if req is None:
                break

            code = req["code"]
            variables = req["variables"]
            timeout = req["timeout"]
            heartbeat_interval = req.get("heartbeat_interval", 1.0)

            result_holder: List[Optional[Dict[str, Any]]] = [None]

            def run_request() -> None:
                result_holder[0] = _execute_single_request(
                    code, variables, heartbeat_interval
                )

            thread = threading.Thread(target=run_request, daemon=True)
            thread.start()
            thread.join(timeout=timeout)

            if thread.is_alive():
                response_queue.put({
                    "status": "error",
                    "result": None,
                    "stdout": "",
                    "execution_time_ms": 0,
                    "error": {
                        "error_type": "TimeoutError",
                        "message": f"Execution exceeded maximum allowed time limit of {timeout} seconds.",
                        "code_with_line_numbers": _format_code_with_line_numbers(code),
                        "hint": "Optimize graph traversals or loops, or simplify problem scope.",
                    },
                })
                break
            else:
                response_queue.put(result_holder[0])

        except Exception as loop_exc:
            try:
                response_queue.put({
                    "status": "error",
                    "result": None,
                    "stdout": "",
                    "execution_time_ms": 0,
                    "error": {
                        "error_type": "WorkerCrash",
                        "message": f"Worker loop failed unexpectedly: {type(loop_exc).__name__}: {loop_exc}",
                        "hint": "The worker process encountered an internal error. It will restart on the next request.",
                    },
                })
            except Exception:
                pass
            break


class PersistentSandboxManager:
    """Manages a long-lived persistent worker process with auto-restart on failure."""

    def __init__(self) -> None:
        self.ctx = multiprocessing.get_context("spawn")
        self.proc: Optional[multiprocessing.Process] = None
        self.request_queue: Optional[multiprocessing.Queue] = None
        self.response_queue: Optional[multiprocessing.Queue] = None

    def _shutdown_worker(self) -> None:
        """Send shutdown signal and cleanly terminate the worker process."""
        if self.request_queue is not None:
            try:
                self.request_queue.put(None)
            except Exception:
                pass
        if self.proc is not None:
            self.proc.join(timeout=2.0)
            if self.proc.is_alive():
                self.proc.terminate()
                self.proc.join(timeout=1.0)
                if self.proc.is_alive():
                    self.proc.kill()
            self.proc = None

    def _ensure_worker(self) -> None:
        if self.proc is None or not self.proc.is_alive():
            self._shutdown_worker()
            self.request_queue = self.ctx.Queue()
            self.response_queue = self.ctx.Queue()
            self.proc = self.ctx.Process(
                target=_worker_loop,
                args=(self.request_queue, self.response_queue),
                daemon=True,
            )
            self.proc.start()

    def execute(
        self,
        code: str,
        variables: Optional[Dict[str, Any]] = None,
        timeout: float = 5.0,
        heartbeat_interval: float = 1.0,
    ) -> Dict[str, Any]:
        self._ensure_worker()
        start_time = time.perf_counter()

        self.request_queue.put({
            "code": code,
            "variables": variables or {},
            "timeout": timeout,
            "heartbeat_interval": heartbeat_interval,
        })

        try:
            return self.response_queue.get(timeout=timeout + 1.0)
        except Empty:
            self._shutdown_worker()
            self.proc = None

            return {
                "status": "error",
                "result": None,
                "stdout": "",
                "execution_time_ms": _elapsed_ms(start_time),
                "error": {
                    "error_type": "TimeoutError",
                    "message": f"Execution exceeded maximum allowed time limit of {timeout} seconds.",
                    "code_with_line_numbers": _format_code_with_line_numbers(code),
                    "hint": "Optimize graph traversals or loops, or simplify problem scope.",
                },
            }


# Singleton Sandbox Instance
SANDBOX_MANAGER = PersistentSandboxManager()


# ---------------------------------------------------------------------------
# Graceful shutdown on parent process exit
# ---------------------------------------------------------------------------
def _shutdown_on_exit() -> None:
    SANDBOX_MANAGER._shutdown_worker()


atexit.register(_shutdown_on_exit)


# ---------------------------------------------------------------------------
# MCP Tool Definition
# ---------------------------------------------------------------------------
def _variables_json_schema(schema: Dict[str, Any]) -> None:
    """Replace the generated schema for ``variables`` in place.

    Pydantic renders ``Optional[Dict[str, Any]]`` as
    ``{"anyOf": [{"type": "object", "additionalProperties": true}, ...]}``.
    A bare boolean is legal JSON Schema ("anything") and Ollama ignores it,
    but llama.cpp compiles the tool list into a decoding grammar and rejects
    boolean sub-schemas outright -- ``Unrecognized schema: true``, HTTP 400
    for the *whole request*, not just calls to this tool. Spelling the value
    type out keeps the wire schema grammar-safe while leaving the runtime
    annotation (and therefore what actually validates) exactly as permissive
    as it was.
    """
    schema.clear()
    schema.update(
        {
            "type": "object",
            "additionalProperties": {
                "anyOf": [
                    {"type": "string"},
                    {"type": "number"},
                    {"type": "boolean"},
                    {"type": "null"},
                    {"type": "array"},
                    {"type": "object"},
                ]
            },
        }
    )


@mcp.tool()
async def execute_math_python(
    code: str,
    variables: Annotated[
        Optional[Dict[str, Any]],
        Field(json_schema_extra=_variables_json_schema),
    ] = None,
) -> str:
    """CRITICAL SYSTEM INSTRUCTION FOR ANALYTICAL, MATHEMATICAL & TEXT TASKS:
    Always run live Python code to analyze data, structure thoughts, or compute deterministic results.
    Use this tool as your primary scratchpad for:

    1. **Precise Math & Science Calculations (`math`, `cmath`, `statistics`, `fractions`):** Algebra, geometry, trigonometry, calculus approximations, physics/engineering formulas, probability distributions, matrix operations, unit conversions, and statistical analysis (mean, variance, standard deviation, regression).
    2. **Constraint Satisfaction & Logic Puzzles (`itertools`, custom loops):** Solving puzzles, seating/shift arrangements, resource allocations, Sudoku, permutations, and rule-based backtracking.
    3. **Advanced Date, Time & Calendar Math (`datetime`, `calendar`):** Computing business-day offsets, project deadlines, relative date spans, leap years, and timezone math.
    4. **Financial & Precision Arithmetic (`decimal`, `fractions`):** Loan amortization schedules, multi-tier tax brackets, compound interest, currency math, and exact decimal operations without floating-point rounding errors.
    5. **Bulk Text Cleaning & Structural Transformation (`re`, `json`):** Reformatting raw CSVs, markdown tables, or unstructured text logs without dropping lines or introducing typos.
    6. **Text & Idea Comparison (`difflib`, `set`, `re`):** Finding common concepts across multiple documents, set intersections (`set(A) & set(B)`), extracting shared keywords, and deduplicating ideas.
    7. **Graph Analysis & Workflow Planning (`networkx`, `nx`):** Topological sorting, finding critical path dependencies (`nx.topological_sort`), identifying bottlenecks, and verifying DAGs (`nx.is_directed_acyclic_graph`).

    Do NOT perform mental math in prose, estimate dates, guess constraint solutions, or attempt to compare long texts purely in prose. Load text/data into 'variables' and execute Python to generate exact, deterministic outputs.

    Args:
        code: Python expression or script to execute.
        variables: Optional dictionary of data, text payloads, or variables to pre-load into scope.

    Returns:
        Structured compact JSON string containing 'status', 'result', 'stdout', 'execution_time_ms', 
        'result_truncated' (bool), 'result_size_bytes', and actionable 'error' details with line numbers 
        if execution fails. Long-running operations emit [HEARTBEAT] markers to stdout every second.
    """
    output_data = SANDBOX_MANAGER.execute(code, variables=variables)
    return json.dumps(output_data, default=str)


def main() -> None:
    mcp.run(transport="stdio")


if __name__ == "__main__":
    multiprocessing.freeze_support()
    main()