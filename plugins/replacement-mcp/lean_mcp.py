# /// script
# dependencies = [
#   "fastmcp",
#   "httpx",
#   "trafilatura",
#   "ddgs",
#   "openpyxl",
#   "python-docx",
#   "pypdf",
#   "pyyaml"
# ]
# ///

import os
import re
import json
import subprocess
from collections import deque
from pathlib import Path
from typing import Literal, Optional, Any
import yaml
from fastmcp import FastMCP

# ---------------------------------------------------------------------------
# Robust YAML config loading
# ---------------------------------------------------------------------------
CONFIG_PATH = Path(__file__).resolve().parent / "tool_prompts.yaml"
if not CONFIG_PATH.exists():
    raise FileNotFoundError(
        f"Missing required configuration file: {CONFIG_PATH}"
    )

with open(CONFIG_PATH, "r", encoding="utf-8") as f:
    CONFIG = yaml.safe_load(f)

PROMPTS  = CONFIG
THRESH   = PROMPTS.get("thresholds", {})

# ---------------------------------------------------------------------------
# Result prefix tags
# ---------------------------------------------------------------------------
PREFIX_OK        = "[OK] "
PREFIX_TRUNCATED = "[TRUNCATED] "

# ---------------------------------------------------------------------------
# Consistent error-handling with recovery hints
# ---------------------------------------------------------------------------
def error(code: str, message: str, detail: Optional[str] = None,
          hint: Optional[str] = None) -> str:
    payload: dict[str, Any] = {"error": True, "code": code, "message": message}
    if detail:
        payload["detail"] = detail
    if hint:
        payload["hint"] = hint
    return f"[ERR:{code}] {json.dumps(payload)}"

# ---------------------------------------------------------------------------
# Initialize MCP Server
# ---------------------------------------------------------------------------
mcp = FastMCP(
    "lean-goose-mcp",
    instructions=PROMPTS.get("server_instructions", "")
)

# ---------------------------------------------------------------------------
# Persistent directories
# ---------------------------------------------------------------------------
CACHE_DIR     = Path.home() / ".cache" / "lean-goose-mcp"
SCRATCH_FILE  = CACHE_DIR / "scratchpad.json"
CACHE_DIR.mkdir(parents=True, exist_ok=True)

# ---------------------------------------------------------------------------
# Helper: strip ANSI escape codes
# ---------------------------------------------------------------------------
def _strip_ansi(text: str) -> str:
    return re.sub(r'\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])', '', text)

# ---------------------------------------------------------------------------
# Helper: density-aware truncation
# ---------------------------------------------------------------------------
def _truncate_text(text: str, density: str, terse_max: int, normal_max: int) -> tuple[str, bool]:
    if density == "terse" and len(text) > terse_max:
        return text[:terse_max], True
    if density == "normal" and len(text) > normal_max:
        return text[:normal_max], True
    return text, False

# ---------------------------------------------------------------------------
# Helper: count words
# ---------------------------------------------------------------------------
def _word_count(text: str) -> int:
    return len(text.split())

# ---------------------------------------------------------------------------
# Helper: load scratchpad JSON
# ---------------------------------------------------------------------------
def _load_scratchpad() -> dict[str, str]:
    if SCRATCH_FILE.exists():
        with open(SCRATCH_FILE, "r", encoding="utf-8") as f:
            return json.load(f)
    return {}

# ---------------------------------------------------------------------------
# Helper: save scratchpad JSON
# ---------------------------------------------------------------------------
def _save_scratchpad(data: dict[str, str]) -> None:
    SCRATCH_FILE.parent.mkdir(parents=True, exist_ok=True)
    with open(SCRATCH_FILE, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)

# ===========================================================================
# Tool A: shell
# ===========================================================================
@mcp.tool(name="lean_shell", description=PROMPTS["shell"]["description"])
def shell(command: str, dry_run: bool = False) -> str:
    if dry_run:
        return f"{PREFIX_OK}[DRY RUN] Would execute: {command}"

    try:
        result = subprocess.run(
            command, shell=True, capture_output=True, text=True, timeout=30
        )
    except subprocess.TimeoutExpired:
        return error(
            "SHELL_TIMEOUT",
            "Command timed out after 30s",
            hint="Try a faster approach, increase timeout, or break the work into smaller commands."
        )

    max_lines = THRESH.get("shell_max_lines", 100)
    keep_head = THRESH.get("shell_keep_head", 30)
    keep_tail = THRESH.get("shell_keep_tail", 30)

    if result.returncode != 0:
        stderr = _strip_ansi(result.stderr)
        lines = stderr.strip().splitlines()
        if len(lines) > keep_tail:
            stderr = "\n".join(lines[-keep_tail:]) + f"\n... [{len(lines) - keep_tail} lines omitted from top]"
        return (
            f"[ERR:SHELL_NONZERO] Exit code: {result.returncode}\n"
            f"stderr (last {keep_tail} lines):\n{stderr}\n"
            f'hint: "Check the command syntax and file paths. Use dry_run=True to preview first."'
        )

    output = result.stdout or ""
    output = _strip_ansi(output)
    lines = output.strip().splitlines()

    if len(lines) <= max_lines:
        return f"{PREFIX_OK}{output}"

    head = "\n".join(lines[:keep_head])
    tail = "\n".join(lines[-keep_tail:])
    omitted = len(lines) - keep_head - keep_tail
    return f"{PREFIX_TRUNCATED}{head}\n... [{omitted} lines omitted] ...\n{tail}"

# ===========================================================================
# Tool B: file_editor
# ===========================================================================
@mcp.tool(name="lean_file_editor", description=PROMPTS["file_editor"]["description"])
def file_editor(
    path: str,
    action: Literal["read", "write", "append"],
    content: Optional[str] = None,
    start_line: int = 1,
    end_line: Optional[int] = None,
    density: Literal["terse", "normal", "verbose"] = "normal",
    dry_run: bool = False,
) -> str:
    resolved = Path(path).resolve()

    if action == "write":
        if dry_run:
            wc = _word_count(content or "")
            return f"{PREFIX_OK}[DRY RUN] Would write {wc} words to {resolved}"
        resolved.parent.mkdir(parents=True, exist_ok=True)
        resolved.write_text(content or "", encoding="utf-8")
        wc = _word_count(content or "")
        return f"{PREFIX_OK}Wrote {wc} words to {resolved}:\n{content}"

    if action == "append":
        if not resolved.exists():
            return error("FILE_NOT_FOUND", "Path does not exist", str(resolved),
                         hint="Check the path spelling. Use analyze_workspace to explore the directory first.")
        if dry_run:
            wc = _word_count(content or "")
            return f"{PREFIX_OK}[DRY RUN] Would append {wc} words to {resolved}"
        with open(resolved, "a", encoding="utf-8") as f:
            f.write(content or "")
        wc = _word_count(content or "")
        return f"{PREFIX_OK}Appended {wc} words to {resolved}"

    # action == "read"
    if not resolved.exists():
        return error("FILE_NOT_FOUND", "Path does not exist", str(resolved),
                     hint="Check the path spelling. Use analyze_workspace to explore the directory first.")

    # Clamp start_line to a valid minimum; the upper-bound clamp (start_line >
    # total_lines) is handled below once the line count is known.
    if start_line < 1:
        start_line = 1

    try:
        if density == "terse":
            # Stream just the first 3 lines instead of reading the whole file.
            head = []
            with open(resolved, "r", encoding="utf-8") as f:
                for i, line in enumerate(f):
                    if i >= 3:
                        break
                    head.append(line.rstrip("\n"))
            return f"{PREFIX_OK}" + "\n".join(head)

        if density == "verbose":
            with open(resolved, "r", encoding="utf-8") as f:
                numbered = [f"{i}: {line.rstrip(chr(10))}" for i, line in enumerate(f, start=1)]
            return f"{PREFIX_OK}" + "\n".join(numbered)

        # normal: paginated. Stream the file once, keeping only the requested
        # window in memory plus a bounded tail buffer (for the "start_line
        # beyond EOF -> clamp to last page" case) instead of materializing
        # every line of a potentially huge file.
        page_size = THRESH.get("file_page_size", 200)
        window_end = end_line or (start_line + page_size - 1)
        window: list[str] = []
        tail: deque[str] = deque(maxlen=page_size)
        total_lines = 0
        with open(resolved, "r", encoding="utf-8") as f:
            for total_lines, raw_line in enumerate(f, start=1):
                line = raw_line.rstrip("\n")
                if start_line <= total_lines <= window_end:
                    window.append(line)
                tail.append(line)

        if start_line > total_lines:
            # Smarter default: clamp to the last page instead of erroring.
            start_line = max(1, total_lines - page_size + 1)
            page_lines = list(tail)
            end = total_lines
        else:
            page_lines = window
            end = min(window_end, total_lines)

        page = "\n".join(page_lines)
        has_more = end < total_lines
        prefix = PREFIX_TRUNCATED if has_more else PREFIX_OK
        result = f"{prefix}Lines {start_line}-{end} of {total_lines}\n{page}"
        if has_more:
            result += f"\n--- More lines available ({total_lines - end} remaining). Use start_line={end + 1} to continue. ---"
        return result
    except PermissionError:
        return error("FILE_PERMISSION", "Cannot read/write path", str(resolved),
                     hint="Check file permissions or try a different location.")
    except Exception as e:
        return error("FILE_PERMISSION", f"Cannot read path: {e}", str(resolved),
                     hint="Check file permissions or try a different location.")

# ===========================================================================
# Tool C: analyze_workspace
# ===========================================================================
@mcp.tool(name="lean_analyze_workspace", description=PROMPTS["analyze_workspace"]["description"])
def analyze_workspace(
    path: str = ".",
    max_depth: Optional[int] = None,
    density: Literal["terse", "normal", "verbose"] = "normal",
) -> str:
    resolved = Path(path).resolve()

    if not resolved.exists():
        return error("PATH_NOT_FOUND", "Directory does not exist", str(resolved),
                     hint="Check the path spelling. Start from a known location like '.' or the user's home directory.")

    # If path is a file, return metadata
    if resolved.is_file():
        stat = resolved.stat()
        from datetime import datetime
        mtime = datetime.fromtimestamp(stat.st_mtime).isoformat()
        return f"{PREFIX_OK}File: {resolved.name} | Size: {stat.st_size} bytes | Modified: {mtime}\nUse file_editor to read this file."

    # Directory behavior
    blacklist = {'.git', 'node_modules', '__pycache__', 'venv', '.venv', 'dist', 'build', '.tox', '.mypy_cache', '.pytest_cache'}
    depth = max_depth if max_depth is not None else THRESH.get("workspace_max_depth", 10)
    max_files = THRESH.get("workspace_max_files", 150)

    file_count = 0
    dir_count = 0
    tree_lines = []
    abort = False
    depth_limited = False

    def walk(current: Path, current_depth: int):
        nonlocal file_count, dir_count, abort, depth_limited
        if abort:
            return
        if current_depth > depth:
            return
        try:
            entries = sorted(current.iterdir(), key=lambda e: (e.is_file(), e.name.lower()))
        except PermissionError:
            return

        indent = "  " * current_depth
        for entry in entries:
            if entry.name in blacklist:
                continue
            if abort:
                return
            if entry.is_dir():
                if density != "terse":
                    # Terse output reports only aggregate counts plus its own
                    # separately-computed top-level listing (below); building
                    # tree_lines entries for it here would just be discarded.
                    tree_lines.append(f"{indent}{entry.name}/")
                dir_count += 1
                if current_depth + 1 > depth:
                    # Would exceed the depth limit; flag as truncated if the
                    # directory actually has (non-blacklisted) children.
                    try:
                        if any(child.name not in blacklist for child in entry.iterdir()):
                            depth_limited = True
                    except PermissionError:
                        pass
                else:
                    walk(entry, current_depth + 1)
            else:
                file_count += 1
                if file_count > max_files:
                    abort = True
                    tree_lines.append(f"{indent}... (file count exceeded {max_files}, stopped)")
                    return
                if density == "verbose":
                    stat = entry.stat()
                    from datetime import datetime
                    mtime = datetime.fromtimestamp(stat.st_mtime).isoformat()
                    tree_lines.append(f"{indent}{entry.name} ({stat.st_size} bytes, {mtime})")
                elif density != "terse":
                    tree_lines.append(f"{indent}{entry.name}")

    walk(resolved, 0)

    if density == "terse":
        return f"{PREFIX_OK}{file_count} files, {dir_count} directories in {resolved}\nTop-level: " + ", ".join(
            e.name for e in sorted(resolved.iterdir()) if e.name not in blacklist and e.is_dir()
        )

    if density == "verbose":
        # Verbose is full detail; only a hard file-count abort counts as truncation.
        prefix = PREFIX_TRUNCATED if abort else PREFIX_OK
    else:
        # normal: also flag truncation when the tree was cut off by depth.
        prefix = PREFIX_TRUNCATED if (abort or depth_limited) else PREFIX_OK
    header = f"{resolved} ({file_count} files, {dir_count} directories)"
    result = "\n".join([header] + tree_lines)
    return f"{prefix}{result}"

# ===========================================================================
# Tool D: fallback_web_search
# ===========================================================================
@mcp.tool(name="lean_fallback_web_search", description=PROMPTS["fallback_web_search"]["description"])
def fallback_web_search(query: str) -> str:
    # `ddgs`, not `duckduckgo_search` — the latter was renamed and its final
    # releases are non-functional against DuckDuckGo's current backend:
    # confirmed real bug, every query returned zero results (surfacing as a
    # bare "No results found for: <query>" no matter what was asked) and the
    # occasional query that did return something got an unrelated
    # multi-language ad page rather than search results. The import is kept
    # lazy, and the call signature and result keys (title/href/body) are
    # identical, so only the module name changes here.
    try:
        from ddgs import DDGS
        results = list(DDGS().text(query, max_results=4))
    except Exception as e:
        return error("SEARCH_FAILED", "DuckDuckGo query failed", str(e),
                     hint="Check your internet connection or try a more specific query.")

    if not results:
        return f"{PREFIX_OK}No results found for: {query}"

    lines = []
    for i, r in enumerate(results, 1):
        title = r.get("title", "")
        href = r.get("href", "")
        snippet = r.get("body", "")
        # Strip tracking params from URL (basic: remove everything after ? and # that looks like tracking)
        clean_url = re.sub(r'\?.*$', '', href)
        lines.append(f"{i}. {title} [{clean_url}]\n{snippet}")

    return f"{PREFIX_OK}" + "\n\n".join(lines)

# ===========================================================================
# Tool E: web_scrape
# ===========================================================================
@mcp.tool(name="lean_web_scrape", description=PROMPTS["web_scrape"]["description"])
def web_scrape(
    url: str,
    include_links: bool = False,
    density: Literal["terse", "normal", "verbose"] = "normal",
) -> str:
    # Smarter default: PDF URL → delegate to pdf_manager
    if url.lower().endswith(".pdf"):
        try:
            response = httpx_get(url, timeout=30)
        except Exception:
            return error("SCRAPE_HTTP_ERROR", "Failed to download PDF", url,
                         hint="The PDF may be behind a login wall or inaccessible.")

        pdf_filename = re.sub(r'[^\w\.\-]', '_', url.split("/")[-1] or "downloaded.pdf")
        pdf_path = CACHE_DIR / pdf_filename
        pdf_path.write_bytes(response.content)
        return f"{PREFIX_OK}[Delegated to pdf_manager] PDF saved to cache as {pdf_filename}. Use pdf_manager.read_text or pdf_manager.read_outline to extract content."

    try:
        import httpx
        response = httpx.get(url, timeout=30, follow_redirects=True)
        response.raise_for_status()
    except httpx.HTTPStatusError as e:
        return error("SCRAPE_HTTP_ERROR", f"HTTP {e.response.status_code}", url,
                     hint="The page may be behind a login wall, blocked, or deleted. Try fallback_web_search to find an alternative source.")
    except httpx.TimeoutException:
        return error("SCRAPE_TIMEOUT", "Request timed out", url,
                     hint="The server is slow or unreachable. Try again or use fallback_web_search for a cached/similar page.")
    except Exception as e:
        return error("SCRAPE_HTTP_ERROR", str(e), url,
                     hint="The page may be behind a login wall, blocked, or deleted. Try fallback_web_search to find an alternative source.")

    html = response.text

    # Save raw HTML to cache
    cache_filename = re.sub(r'[^\w\.\-]', '_', url.split("/")[-1] or "index.html") or "page.html"
    cache_path = CACHE_DIR / cache_filename
    cache_path.write_text(html, encoding="utf-8")

    # Extract with trafilatura
    try:
        import trafilatura
        extracted = trafilatura.extract(html, output_format='markdown', include_links=include_links)
    except Exception:
        extracted = None

    if not extracted or not extracted.strip():
        return error("SCRAPE_EMPTY", "No extractable content found", url,
                     hint="The page may be a JavaScript SPA or behind a paywall. Try a different URL or use fallback_web_search.")

    # Collapse whitespace
    extracted = re.sub(r'\n{3,}', '\n\n', extracted.strip())

    terse_cap = THRESH.get("terse_char_cap", 500)
    normal_cap = THRESH.get("scrape_max_chars", 12000)

    if density == "terse" and len(extracted) > terse_cap:
        return f"{PREFIX_TRUNCATED}{extracted[:terse_cap]}"

    if density == "normal" and len(extracted) > normal_cap:
        return f"{PREFIX_TRUNCATED}{extracted[:normal_cap]}"

    return f"{PREFIX_OK}{extracted}"

# ---------------------------------------------------------------------------
# Reusable httpx_get helper used by web_scrape
# ---------------------------------------------------------------------------
def httpx_get(url: str, timeout: int = 30):
    import httpx
    return httpx.get(url, timeout=timeout, follow_redirects=True)

# ===========================================================================
# Tool F: excel_manager
# ===========================================================================
@mcp.tool(name="lean_excel_manager", description=PROMPTS["excel_manager"]["description"])
def excel_manager(
    path: str,
    action: Literal["inspect", "read_rows", "write_rows"],
    sheet: Optional[str] = None,
    range_box: Optional[str] = None,
    csv_data: Optional[str] = None,
    dry_run: bool = False,
) -> str:
    resolved = Path(path).resolve()
    if not resolved.exists():
        return error("XLSX_NOT_FOUND", "Spreadsheet does not exist", str(resolved),
                     hint="Check the file path and extension. Use analyze_workspace to locate .xlsx files.")

    import openpyxl

    try:
        wb = openpyxl.load_workbook(resolved)
    except Exception as e:
        return error("XLSX_NOT_FOUND", f"Cannot open workbook: {e}", str(resolved),
                     hint="The file may be corrupted or not a valid .xlsx file.")

    # Smarter default: find first non-empty sheet
    def _resolve_sheet():
        if sheet:
            if sheet not in wb.sheetnames:
                wb.close()
                return error("XLSX_BAD_SHEET", f"Sheet '{sheet}' not found", str(resolved),
                             hint="Use excel_manager.inspect to see available sheet names.")
            return sheet
        for s in wb.sheetnames:
            ws = wb[s]
            if ws.max_row and ws.max_row > 0:
                return s
        return wb.sheetnames[0] if wb.sheetnames else None

    ws_name = _resolve_sheet()
    if isinstance(ws_name, str) and ws_name.startswith("[ERR:"):
        return ws_name

    ws = wb[ws_name] if ws_name else None
    if ws is None:
        wb.close()
        return error("XLSX_BAD_SHEET", "No sheets found", str(resolved),
                     hint="The workbook appears to be empty.")

    if action == "inspect":
        first_rows = list(ws.iter_rows(max_row=1))
        headers = [cell.value for cell in first_rows[0]] if first_rows else []
        info = (
            f"Sheets: {wb.sheetnames}\n"
            f"Active sheet: {ws_name}\n"
            f"Columns: {headers}\n"
            f"Dimensions: {ws.dimensions}\n"
            f"Rows: {ws.max_row}, Cols: {ws.max_column}"
        )
        wb.close()
        return f"{PREFIX_OK}{info}"

    if action == "read_rows":
        iter_kwargs = {"values_only": True}
        if range_box:
            try:
                min_col, min_row, max_col, max_row = openpyxl.utils.cell.range_boundaries(range_box)
            except ValueError:
                wb.close()
                return error("XLSX_BAD_RANGE", f"Invalid range '{range_box}'", str(resolved),
                             hint="Use A1 notation like 'A1:D50'. Check that rows/cols exist in the sheet.")
            if min_row > (ws.max_row or 0) or min_col > (ws.max_column or 0):
                wb.close()
                return error("XLSX_BAD_RANGE",
                             f"Range '{range_box}' is out of bounds for sheet '{ws_name}' "
                             f"({ws.max_row}x{ws.max_column})",
                             str(resolved),
                             hint="Use A1 notation like 'A1:D50'. Check that rows/cols exist in the sheet.")
            iter_kwargs.update(
                min_row=min_row,
                max_row=min(max_row, ws.max_row),
                min_col=min_col,
                max_col=min(max_col, ws.max_column),
            )
        rows = []
        for row in ws.iter_rows(**iter_kwargs):
            rows.append(",".join(str(c) if c is not None else "" for c in row))
        wb.close()
        return f"{PREFIX_OK}" + "\n".join(rows)

    if action == "write_rows":
        if dry_run:
            if csv_data:
                row_count = len(csv_data.strip().splitlines())
            else:
                row_count = 0
            wb.close()
            return f"{PREFIX_OK}[DRY RUN] Would write {row_count} rows to sheet '{ws_name}'"

        if not csv_data:
            wb.close()
            return error("XLSX_MISSING_DATA", "No csv_data provided for write_rows", str(resolved),
                         hint="Provide csv_data as comma-separated lines for write_rows.")

        rows_written = 0
        for line in csv_data.strip().splitlines():
            values = [v.strip() for v in line.split(",")]
            ws.append(values)
            rows_written += 1

        wb.save(resolved)
        wb.close()
        return f"{PREFIX_OK}{json.dumps({'status': 'success', 'rows_updated': rows_written})}"

    wb.close()
    return ""

# ===========================================================================
# Tool G: word_manager
# ===========================================================================
@mcp.tool(name="lean_word_manager", description=PROMPTS["word_manager"]["description"])
def word_manager(
    path: str,
    action: Literal["read_outline", "read_text", "write_doc"],
    doc_text: Optional[str] = None,
    write_mode: Literal["create", "append"] = "create",
    title: Optional[str] = None,
    density: Literal["terse", "normal", "verbose"] = "normal",
) -> str:
    from docx import Document

    resolved = Path(path).resolve()

    def _safe_style_name(para) -> str:
        # Some exporters (notably Google Docs) can emit paragraphs whose
        # style ID isn't present in the document's stylesheet, which makes
        # python-docx raise KeyError on .style.name access.
        try:
            return para.style.name
        except Exception:
            return "Normal"

    if action in ("read_outline", "read_text"):
        if not resolved.exists():
            return error("DOCX_NOT_FOUND", "Document does not exist", str(resolved),
                         hint="Check the file path. Use write_mode='create' to make a new document.")
        try:
            doc = Document(str(resolved))
        except Exception as e:
            return error("DOCX_CORRUPT", f"Cannot parse document: {e}", str(resolved),
                         hint="The file may be damaged or in an unsupported format. Try opening it manually to verify.")

        try:
            if action == "read_outline":
                lines = []
                for para in doc.paragraphs:
                    style_name = _safe_style_name(para)
                    if style_name.startswith("Heading"):
                        level = style_name.replace("Heading ", "")
                        if level.isdigit() and 1 <= int(level) <= 3:
                            indent = "  " * (int(level) - 1)
                            lines.append(f"{indent}{para.text}")
                return f"{PREFIX_OK}" + ("\n".join(lines) if lines else "No headings found.")

            # read_text
            all_text = "\n".join(p.text for p in doc.paragraphs if p.text.strip())

            if not all_text.strip():
                return f"{PREFIX_OK}[Empty document]"

            if density == "terse":
                capped = all_text[:THRESH.get("terse_char_cap", 500)]
                return f"{PREFIX_TRUNCATED}{capped}"

            if density == "verbose":
                numbered = "\n".join(
                    f"{i}. [{_safe_style_name(p)}] {p.text}"
                    for i, p in enumerate(doc.paragraphs, 1)
                    if p.text.strip()
                )
                return f"{PREFIX_OK}{numbered}"

            return f"{PREFIX_OK}{all_text}"
        except Exception as e:
            return error("DOCX_CORRUPT", f"Cannot read document contents: {e}", str(resolved),
                         hint="The file may use unsupported formatting. Try opening it manually to verify.")

    # action == "write_doc"
    added = 0
    if write_mode == "create":
        doc = Document()
        if title:
            doc.add_heading(title, level=0)
            added += 1
    else:
        if not resolved.exists():
            return error("DOCX_NOT_FOUND", "Document does not exist", str(resolved),
                         hint="Check the file path. Use write_mode='create' to make a new document.")
        try:
            doc = Document(str(resolved))
        except Exception as e:
            return error("DOCX_CORRUPT", f"Cannot parse document: {e}", str(resolved),
                         hint="The file may be damaged or in an unsupported format.")

    if doc_text:
        for line in doc_text.strip().splitlines():
            stripped = line.strip()
            if stripped.startswith("### "):
                doc.add_heading(stripped[4:], level=3)
            elif stripped.startswith("## "):
                doc.add_heading(stripped[3:], level=2)
            elif stripped.startswith("# "):
                doc.add_heading(stripped[2:], level=1)
            else:
                doc.add_paragraph(stripped)
            added += 1

    resolved.parent.mkdir(parents=True, exist_ok=True)
    doc.save(str(resolved))

    payload = json.dumps({
        "status": "success",
        "mode": write_mode,
        "paragraphs_written": added,
        "path": str(resolved),
    })
    return f"{PREFIX_OK}{payload}"

# ===========================================================================
# Tool H: pdf_manager
# ===========================================================================
@mcp.tool(name="lean_pdf_manager", description=PROMPTS["pdf_manager"]["description"])
def pdf_manager(
    path: str,
    action: Literal["read_outline", "read_text"],
    density: Literal["terse", "normal", "verbose"] = "normal",
) -> str:
    resolved = Path(path).resolve()
    if not resolved.exists():
        return error("PDF_NOT_FOUND", "PDF does not exist", str(resolved),
                     hint="Check the file path. Use analyze_workspace to locate .pdf files.")

    from pypdf import PdfReader

    try:
        reader = PdfReader(str(resolved))
    except Exception as e:
        msg = str(e).lower()
        if "encrypted" in msg or "password" in msg:
            return error("PDF_ENCRYPTED", "PDF is password-protected", str(resolved),
                         hint="Ask the user for the password. This tool cannot crack passwords.")
        return error("PDF_CORRUPT", f"Cannot parse PDF: {e}", str(resolved),
                     hint="The file may be damaged. Try opening it manually or use a different copy.")

    if reader.is_encrypted:
        return error("PDF_ENCRYPTED", "PDF is password-protected", str(resolved),
                     hint="Ask the user for the password. This tool cannot crack passwords.")

    if action == "read_outline":
        outline = reader.outline or []
        lines = []

        def _walk_outline(items, depth=0):
            for item in items:
                if isinstance(item, list):
                    _walk_outline(item, depth + 1)
                elif hasattr(item, "title") and hasattr(item, "page"):
                    page_num = reader.pages.index(item.page) + 1 if item.page else "?"
                    lines.append(f"{'  ' * depth}{item.title} (p. {page_num})")
                elif hasattr(item, "title"):
                    lines.append(f"{'  ' * depth}{item.title}")

        _walk_outline(outline)
        return f"{PREFIX_OK}" + ("\n".join(lines) if lines else "No outline/bookmarks found.")

    # read_text
    num_pages = len(reader.pages)
    terse_cap = THRESH.get("terse_char_cap", 500)
    normal_cap = THRESH.get("scrape_max_chars", 12000)

    all_text_parts = []
    accumulated_chars = 0
    capped = False
    for i in range(num_pages):
        if density == "terse" and i >= 3:
            break
        if density == "normal" and accumulated_chars > normal_cap:
            # Already have more than enough to satisfy the cap below;
            # skip extracting text from the remaining pages entirely.
            capped = True
            break
        page_text = reader.pages[i].extract_text() or ""
        if density == "terse" and len(page_text) > terse_cap:
            page_text = page_text[:terse_cap]
        part = f"--- Page {i+1} ---\n{page_text.strip()}"
        all_text_parts.append(part)
        accumulated_chars += len(part)

    full_text = "\n\n".join(all_text_parts)
    total_chars = len(full_text)

    if density == "terse":
        return f"{PREFIX_TRUNCATED}{full_text}"
    if density == "normal" and (capped or total_chars > normal_cap):
        return f"{PREFIX_TRUNCATED}{full_text[:normal_cap]}"

    return f"{PREFIX_OK}{full_text}"

# ===========================================================================
# Tool I: cache_manager
# ===========================================================================
@mcp.tool(name="lean_cache_manager", description=PROMPTS["cache_manager"]["description"])
def cache_manager(
    command: Literal["list", "view", "delete", "clear"],
    filename: Optional[str] = None,
) -> str:
    if command == "list":
        if not CACHE_DIR.exists():
            return f"{PREFIX_OK}[Empty cache]"
        entries = []
        for f in sorted(CACHE_DIR.iterdir()):
            if f.is_file():
                stat = f.stat()
                from datetime import datetime
                mtime = datetime.fromtimestamp(stat.st_mtime).isoformat()
                entries.append(f"{f.name:40s} {stat.st_size:>8d} bytes  {mtime}")
        if not entries:
            return f"{PREFIX_OK}[Empty cache]"
        return f"{PREFIX_OK}" + "\n".join(entries)

    if command == "view":
        if not filename:
            return error("CACHE_MISS", "No filename specified",
                         hint="Use cache_manager.list to see available cached files.")
        cache_file = CACHE_DIR / filename
        if not cache_file.exists():
            return error("CACHE_MISS", f"File '{filename}' not in cache",
                         hint="Use cache_manager.list to see available cached files.")
        max_chars = THRESH.get("scrape_max_chars", 12000)
        # Read one char past the cap instead of the whole file, just enough
        # to tell whether the content needed truncating.
        with open(cache_file, "r", encoding="utf-8") as f:
            text = f.read(max_chars + 1)
        if len(text) > max_chars:
            return f"{PREFIX_TRUNCATED}{text[:max_chars]}"
        return f"{PREFIX_OK}{text}"

    if command == "delete":
        if not filename:
            return error("CACHE_MISS", "No filename specified",
                         hint="Use cache_manager.list to see available cached files.")
        cache_file = CACHE_DIR / filename
        if not cache_file.exists():
            return error("CACHE_MISS", f"File '{filename}' not in cache",
                         hint="Use cache_manager.list to see available cached files.")
        cache_file.unlink()
        return f"{PREFIX_OK}{json.dumps({'status': 'deleted', 'filename': filename})}"

    if command == "clear":
        if not CACHE_DIR.exists():
            return f'{PREFIX_OK}{{"status": "cleared", "files_removed": 0}}'
        count = 0
        for f in CACHE_DIR.iterdir():
            if f.is_file():
                f.unlink()
                count += 1
        return f'{PREFIX_OK}{{"status": "cleared", "files_removed": {count}}}'

    return ""

# ===========================================================================
# Tool J: scratchpad
# ===========================================================================
@mcp.tool(name="lean_scratchpad", description=PROMPTS["scratchpad"]["description"])
def scratchpad(
    command: Literal["set", "get", "delete", "list"],
    key: Optional[str] = None,
    value: Optional[str] = None,
) -> str:
    data = _load_scratchpad()
    max_keys = THRESH.get("scratchpad_max_keys", 50)

    if command == "set":
        if key is None:
            return error("SCRATCHPAD_MISS", "No key specified",
                         hint="Provide a key name to store the value under.")
        if key not in data and len(data) >= max_keys:
            return error("SCRATCHPAD_FULL", f"Max {max_keys} keys reached",
                         hint="Use scratchpad.delete to remove unused entries, then retry.")
        data[key] = value or ""
        _save_scratchpad(data)
        return f"{PREFIX_OK}Stored."

    if command == "get":
        if key is None:
            return error("SCRATCHPAD_MISS", "No key specified",
                         hint="Provide the key name to retrieve.")
        if key not in data:
            return error("SCRATCHPAD_MISS", f"No entry for '{key}'",
                         hint="Use scratchpad.list to see all keys.")
        return f"{PREFIX_OK}{data[key]}"

    if command == "delete":
        if key is None:
            return error("SCRATCHPAD_MISS", "No key specified",
                         hint="Provide the key name to delete.")
        if key not in data:
            return error("SCRATCHPAD_MISS", f"No entry for '{key}'",
                         hint="Use scratchpad.list to see all keys.")
        del data[key]
        _save_scratchpad(data)
        return f"{PREFIX_OK}Deleted."

    if command == "list":
        if not data:
            return f"{PREFIX_OK}[Empty scratchpad]"
        return f"{PREFIX_OK}" + ", ".join(sorted(data.keys()))

    return ""

# ===========================================================================
# Entry point
# ===========================================================================
def main() -> None:
    mcp.run()


if __name__ == "__main__":
    main()