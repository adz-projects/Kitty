# /// script
# dependencies = [
#   "fastmcp",
#   "pyyaml"
# ]
# ///

import re
import json
import subprocess
from pathlib import Path
from typing import Optional, Any, Dict, List
import yaml
from fastmcp import FastMCP

# Word, PDF, Excel, web-scrape, and DuckDuckGo-search tools live in
# `kitty-docs-web` (PDF/Excel/web/DDG) and `kitty-tools` (Word) now — see
# `docs/PLUGINS.md`. Keeping them here duplicated the same tool *name* across
# two MCP servers, which silently shadows one registration in BigTiny's
# name-keyed `_tool_registry` (last-connected wins) — a materially worse bug
# than a brief migration window with the tool unavailable from this server.

# ---------------------------------------------------------------------------
# Robust YAML config loading
# ---------------------------------------------------------------------------
CONFIG_PATH = Path(__file__).resolve().parent / "tool_prompts.yaml"
if CONFIG_PATH.exists():
    with open(CONFIG_PATH, "r", encoding="utf-8") as f:
        CONFIG = yaml.safe_load(f)
else:
    CONFIG = {}

PROMPTS = CONFIG
THRESH = PROMPTS.get("thresholds", {})

# ---------------------------------------------------------------------------
# Standardized JSON Response Helpers & Dynamic Error Recovery Hints (Item 5)
# ---------------------------------------------------------------------------
def success_response(
    data: Any,
    message: Optional[str] = None,
    truncated: bool = False,
    metadata: Optional[Dict[str, Any]] = None,
) -> str:
    """Returns a standardized JSON success response."""
    payload: Dict[str, Any] = {
        "status": "success",
        "truncated": truncated,
        "data": data,
    }
    if message:
        payload["message"] = message
    if metadata:
        payload["metadata"] = metadata
    return json.dumps(payload, indent=2, ensure_ascii=False)


def error_response(
    code: str,
    message: str,
    detail: Optional[str] = None,
    hint: Optional[str] = None,
) -> str:
    """Returns a standardized JSON error response with automated recovery hints."""
    payload: Dict[str, Any] = {
        "status": "error",
        "error_code": code,
        "message": message,
    }
    if detail:
        payload["detail"] = detail

    # Item 5: Automatic recovery hints to help small models self-correct
    if not hint:
        if "NOT_FOUND" in code or "MISSING" in code:
            hint = "Verify path spelling or call lean_analyze_workspace to check available files."
        elif "CORRUPT" in code or "PARSE" in code:
            hint = "File may be damaged or password-protected. Verify format."
        elif "BAD_RANGE" in code or "OUT_OF_BOUNDS" in code:
            hint = "Inspect dimensions or line counts before specifying bounds."
        elif "TARGET_NOT_FOUND" in code:
            hint = "Use lean_file_read first to confirm exact string formatting or line numbers."
        elif "SEARCH" in code or "SCRAPE" in code:
            hint = "Broaden search keywords or check domain connectivity."

    if hint:
        payload["hint"] = hint
    return json.dumps(payload, indent=2, ensure_ascii=False)


# ---------------------------------------------------------------------------
# In-Tool Keyword RAG Helper (Item 3)
# ---------------------------------------------------------------------------
def _filter_by_query(
    items: List[str], query: Optional[str] = None, max_results: int = 50
) -> tuple[List[str], bool]:
    """Filters lines or paragraphs by keyword match score."""
    if not query or not query.strip():
        return items[:max_results], len(items) > max_results

    query_words = set(re.findall(r"\w+", query.lower()))
    if not query_words:
        return items[:max_results], len(items) > max_results

    scored = []
    for idx, item in enumerate(items):
        item_words = set(re.findall(r"\w+", item.lower()))
        score = len(query_words.intersection(item_words))
        if score > 0:
            scored.append((score, idx, item))

    if not scored:
        fallback = [f"[No direct matches for query '{query}'. Showing top section]"] + items[:max_results]
        return fallback, len(items) > max_results

    scored.sort(key=lambda x: x[0], reverse=True)
    results = [item for _, _, item in scored[:max_results]]
    return results, len(scored) > max_results


# ---------------------------------------------------------------------------
# Initialize MCP Server
# ---------------------------------------------------------------------------
mcp = FastMCP("lean-goose-mcp", instructions=PROMPTS.get("server_instructions", ""))

# ---------------------------------------------------------------------------
# Persistent directories
# ---------------------------------------------------------------------------
CACHE_DIR = Path.home() / ".cache" / "lean-goose-mcp"
SCRATCH_FILE = CACHE_DIR / "scratchpad.json"
CACHE_DIR.mkdir(parents=True, exist_ok=True)


# ---------------------------------------------------------------------------
# Helper functions
# ---------------------------------------------------------------------------
def _strip_ansi(text: str) -> str:
    return re.sub(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])", "", text)


def _load_scratchpad() -> dict[str, str]:
    if SCRATCH_FILE.exists():
        with open(SCRATCH_FILE, "r", encoding="utf-8") as f:
            return json.load(f)
    return {}


def _save_scratchpad(data: dict[str, str]) -> None:
    SCRATCH_FILE.parent.mkdir(parents=True, exist_ok=True)
    with open(SCRATCH_FILE, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)


# ===========================================================================
# System Tools: shell & analyze_workspace
# ===========================================================================
@mcp.tool(name="lean_shell")
def shell(command: str, dry_run: bool = False) -> str:
    """Runs a shell command and returns truncated stdout/stderr. Set dry_run=True to preview without executing."""
    if dry_run:
        return success_response({"command": command}, message="[DRY RUN] Command not executed.")

    try:
        result = subprocess.run(
            command, shell=True, capture_output=True, text=True, timeout=30
        )
    except subprocess.TimeoutExpired:
        return error_response(
            "SHELL_TIMEOUT",
            "Command timed out after 30s",
            hint="Try a faster approach, increase timeout, or break work into smaller commands.",
        )

    max_lines = THRESH.get("shell_max_lines", 100)
    keep_head = THRESH.get("shell_keep_head", 30)
    keep_tail = THRESH.get("shell_keep_tail", 30)

    if result.returncode != 0:
        stderr = _strip_ansi(result.stderr)
        lines = stderr.strip().splitlines()
        if len(lines) > keep_tail:
            stderr = "\n".join(lines[-keep_tail:])
        return error_response(
            "SHELL_NONZERO",
            f"Exit code {result.returncode}",
            detail=stderr,
        )

    output = _strip_ansi(result.stdout or "")
    lines = output.strip().splitlines()
    truncated = len(lines) > max_lines

    if truncated:
        head = "\n".join(lines[:keep_head])
        tail = "\n".join(lines[-keep_tail:])
        output = f"{head}\n... [{len(lines) - keep_head - keep_tail} lines omitted] ...\n{tail}"

    return success_response(
        output, truncated=truncated, metadata={"returncode": result.returncode}
    )


@mcp.tool(name="lean_analyze_workspace")
def analyze_workspace(path: str = ".", max_depth: Optional[int] = None) -> str:
    """Lists files and folders under path (or returns metadata if path is a file)."""
    resolved = Path(path).resolve()
    if not resolved.exists():
        return error_response("PATH_NOT_FOUND", "Directory does not exist", str(resolved))

    if resolved.is_file():
        stat = resolved.stat()
        return success_response(
            {
                "type": "file",
                "name": resolved.name,
                "size_bytes": stat.st_size,
                "path": str(resolved),
            }
        )

    blacklist = {".git", "node_modules", "__pycache__", "venv", ".venv", "dist", "build", ".tox"}
    depth = max_depth if max_depth is not None else THRESH.get("workspace_max_depth", 10)
    max_files = THRESH.get("workspace_max_files", 150)

    files, dirs = [], []
    abort = False

    def walk(current: Path, current_depth: int):
        nonlocal abort
        if abort or current_depth > depth:
            return
        try:
            entries = sorted(current.iterdir(), key=lambda e: (e.is_file(), e.name.lower()))
        except PermissionError:
            return

        for entry in entries:
            if entry.name in blacklist or abort:
                continue
            rel_path = str(entry.relative_to(resolved))
            if entry.is_dir():
                dirs.append(rel_path)
                walk(entry, current_depth + 1)
            else:
                files.append(rel_path)
                if len(files) >= max_files:
                    abort = True
                    return

    walk(resolved, 0)
    return success_response(
        {"files": files, "directories": dirs},
        truncated=abort,
        metadata={"total_files": len(files), "total_directories": len(dirs), "root": str(resolved)},
    )


# ===========================================================================
# File Tools (Item 1: Split Dedicated Tools, Item 3: RAG, Item 4: Line Replacement)
# ===========================================================================
@mcp.tool(name="lean_file_read")
def file_read(
    path: str,
    start_line: int = 1,
    end_line: Optional[int] = None,
    query: Optional[str] = None,
) -> str:
    """Reads lines from a text file with line numbers. Supports query filtering (Item 3)."""
    resolved = Path(path).resolve()
    if not resolved.exists():
        return error_response("FILE_NOT_FOUND", "Path does not exist", str(resolved))

    try:
        lines = resolved.read_text(encoding="utf-8").splitlines()
        total_lines = len(lines)

        if query and query.strip():
            # Item 3: In-tool query filtering on numbered lines
            numbered_lines = [f"{idx}: {line}" for idx, line in enumerate(lines, start=1)]
            filtered, truncated = _filter_by_query(numbered_lines, query)
            return success_response(
                "\n".join(filtered),
                truncated=truncated,
                metadata={"total_lines": total_lines, "filtered_by_query": query},
            )

        start_line = max(1, start_line)
        page_size = THRESH.get("file_page_size", 200)
        window_end = end_line or (start_line + page_size - 1)
        actual_end = min(window_end, total_lines)

        page_lines = lines[start_line - 1 : actual_end]
        numbered_output = "\n".join(
            f"{idx}: {line}" for idx, line in enumerate(page_lines, start=start_line)
        )
        has_more = actual_end < total_lines

        return success_response(
            numbered_output,
            truncated=has_more,
            metadata={
                "start_line": start_line,
                "end_line": actual_end,
                "total_lines": total_lines,
                "has_more": has_more,
            },
        )
    except Exception as e:
        return error_response("FILE_READ_ERROR", f"Cannot read file: {e}", str(resolved))


@mcp.tool(name="lean_file_write")
def file_write(path: str, content: str, dry_run: bool = False) -> str:
    """Overwrites (or creates) a text file with the given content."""
    resolved = Path(path).resolve()
    if dry_run:
        return success_response({"path": str(resolved)}, message="[DRY RUN] Would write file.")
    resolved.parent.mkdir(parents=True, exist_ok=True)
    resolved.write_text(content, encoding="utf-8")
    return success_response(
        {"path": str(resolved), "words": len(content.split())},
        message="File written successfully.",
    )


@mcp.tool(name="lean_file_append")
def file_append(path: str, content: str, dry_run: bool = False) -> str:
    """Appends content to the end of an existing text file."""
    resolved = Path(path).resolve()
    if not resolved.exists():
        return error_response("FILE_NOT_FOUND", "Path does not exist", str(resolved))
    if dry_run:
        return success_response({"path": str(resolved)}, message="[DRY RUN] Would append to file.")
    with open(resolved, "a", encoding="utf-8") as f:
        f.write(content)
    return success_response(
        {"path": str(resolved), "appended_words": len(content.split())},
        message="Content appended successfully.",
    )


@mcp.tool(name="lean_file_replace_str")
def file_replace_str(path: str, old_str: str, new_str: str, dry_run: bool = False) -> str:
    """Replaces exact string occurrences in a file."""
    resolved = Path(path).resolve()
    if not resolved.exists():
        return error_response("FILE_NOT_FOUND", "Path does not exist", str(resolved))

    file_text = resolved.read_text(encoding="utf-8")
    occurrences = file_text.count(old_str)
    if occurrences == 0:
        return error_response(
            "TARGET_NOT_FOUND",
            "Target string 'old_str' was not found in the file.",
        )

    if dry_run:
        return success_response(
            {"occurrences": occurrences, "path": str(resolved)},
            message=f"[DRY RUN] Would replace {occurrences} occurrence(s).",
        )

    updated_text = file_text.replace(old_str, new_str)
    resolved.write_text(updated_text, encoding="utf-8")
    return success_response(
        {"path": str(resolved), "replacements_made": occurrences},
        message=f"Successfully replaced {occurrences} occurrence(s).",
    )


@mcp.tool(name="lean_file_replace_lines")
def file_replace_lines(
    path: str,
    start_line: int,
    end_line: int,
    new_content: str,
    dry_run: bool = False,
) -> str:
    """Item 4: Replaces a specific line range (1-indexed, inclusive) with new content."""
    resolved = Path(path).resolve()
    if not resolved.exists():
        return error_response("FILE_NOT_FOUND", "Path does not exist", str(resolved))

    lines = resolved.read_text(encoding="utf-8").splitlines()
    total_lines = len(lines)

    if start_line < 1 or start_line > total_lines or end_line < start_line:
        return error_response(
            "OUT_OF_BOUNDS",
            f"Invalid line range {start_line}-{end_line} for file with {total_lines} lines.",
        )

    actual_end = min(end_line, total_lines)

    if dry_run:
        return success_response(
            {"start_line": start_line, "end_line": actual_end, "total_lines": total_lines},
            message="[DRY RUN] Would replace specified line range.",
        )

    new_lines = new_content.splitlines() if new_content else []
    lines[start_line - 1 : actual_end] = new_lines

    resolved.write_text("\n".join(lines) + ("\n" if lines else ""), encoding="utf-8")
    return success_response(
        {
            "path": str(resolved),
            "replaced_range": f"{start_line}-{actual_end}",
            "lines_removed": actual_end - start_line + 1,
            "lines_added": len(new_lines),
            "new_total_lines": len(lines),
        },
        message="Line range replaced successfully.",
    )


# ===========================================================================
# Cache Manager Tools (Item 1: Split Dedicated Tools)
# ===========================================================================
@mcp.tool(name="lean_cache_list")
def cache_list() -> str:
    """Lists files currently stored in the scratch cache directory with their sizes."""
    if not CACHE_DIR.exists():
        return success_response([])
    entries = [
        {"filename": f.name, "size_bytes": f.stat().st_size}
        for f in sorted(CACHE_DIR.iterdir())
        if f.is_file()
    ]
    return success_response(entries)


@mcp.tool(name="lean_cache_view")
def cache_view(filename: str) -> str:
    """Reads the text content of a file previously stored in the scratch cache directory."""
    file_path = CACHE_DIR / filename
    if not file_path.exists():
        return error_response("CACHE_MISS", f"File '{filename}' not found.")
    text = file_path.read_text(encoding="utf-8")
    return success_response(text)


@mcp.tool(name="lean_cache_delete")
def cache_delete(filename: str) -> str:
    """Deletes a single file from the scratch cache directory."""
    file_path = CACHE_DIR / filename
    if file_path.exists():
        file_path.unlink()
        return success_response({"deleted": filename})
    return error_response("CACHE_MISS", f"File '{filename}' not found.")


@mcp.tool(name="lean_cache_clear")
def cache_clear() -> str:
    """Deletes every file in the scratch cache directory and returns how many were removed."""
    count = 0
    if CACHE_DIR.exists():
        for f in CACHE_DIR.iterdir():
            if f.is_file():
                f.unlink()
                count += 1
    return success_response({"files_removed": count})


# ===========================================================================
# Scratchpad Tools (Item 1: Split Dedicated Tools)
# ===========================================================================
@mcp.tool(name="lean_scratchpad_set")
def scratchpad_set(key: str, value: str) -> str:
    """Stores a key/value pair in the persistent scratchpad for recall across turns."""
    data = _load_scratchpad()
    data[key] = value
    _save_scratchpad(data)
    return success_response({"key": key}, message="Stored successfully.")


@mcp.tool(name="lean_scratchpad_get")
def scratchpad_get(key: str) -> str:
    """Retrieves a previously stored scratchpad value by key."""
    data = _load_scratchpad()
    if key not in data:
        return error_response("KEY_NOT_FOUND", f"Key '{key}' not in scratchpad.")
    return success_response({"key": key, "value": data[key]})


@mcp.tool(name="lean_scratchpad_delete")
def scratchpad_delete(key: str) -> str:
    """Deletes a key from the persistent scratchpad."""
    data = _load_scratchpad()
    if key not in data:
        return error_response("KEY_NOT_FOUND", f"Key '{key}' not in scratchpad.")
    del data[key]
    _save_scratchpad(data)
    return success_response({"deleted_key": key})


@mcp.tool(name="lean_scratchpad_list")
def scratchpad_list() -> str:
    """Lists all keys currently stored in the persistent scratchpad."""
    data = _load_scratchpad()
    return success_response(list(data.keys()))


# ===========================================================================
# Entry point
# ===========================================================================
def main() -> None:
    mcp.run()


if __name__ == "__main__":
    main()