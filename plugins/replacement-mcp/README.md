# Lean Goose MCP Server

A single-file Python MCP server that replaces Goose's `developer` + `computer_controller`
extensions with 10 lean, context-optimized tools for a local 35B model.

## Quick Start

```bash
cd lean-goose-mcp
uv run lean_mcp.py
```

## Files

| File | Purpose |
|------|---------|
| `PROJECT_PLAN.md` | Full technical specification |
| `LAUNCH.md` | Step-by-step build and deployment guide |
| `tool_prompts.yaml` | Tool descriptions, thresholds, and server instructions |
| `lean_mcp.py` | Single-file MCP server with PEP 723 inline dependencies |

## Tools

| Category | Tool | Purpose |
|----------|------|---------|
| **System** | `shell` | Safe terminal execution with dry-run preview |
| | `file_editor` | Read/write/append with pagination and density control |
| | `analyze_workspace` | Directory scanning with smart file detection |
| **Web** | `fallback_web_search` | DuckDuckGo search |
| | `web_scrape` | Article extraction to clean Markdown |
| **Documents** | `excel_manager` | Spreadsheet inspect/read/write |
| | `word_manager` | DOCX read/write with heading styles |
| | `pdf_manager` | PDF text and outline extraction |
| **State** | `cache_manager` | Manage scraped content cache |
| | `scratchpad` | Persistent cross-turn key-value store |

## Key Design Decisions

- **Prefix tags** (`[OK]`, `[TRUNCATED]`, `[ERR:CODE]`) on every result — parseable in one token
- **Recovery hints** in every error — prevents error loops
- **Density control** (`terse`/`normal`/`verbose`) — model self-regulates context spend
- **Smarter defaults** — auto-delegate PDFs, auto-select sheets, clamp out-of-range lines
- **Tool grouping** (`System`/`Web`/`Documents`/`State`) — reduces tool-selection overhead
- **~1,000 words injected** into system prompt — under 5% of a 32K context window

## Dependencies

All declared via PEP 723 inline script header:

- `fastmcp` — MCP server framework
- `httpx` — HTTP client for scraping
- `trafilatura` — HTML-to-Markdown extraction
- `ddgs` — Web search fallback (successor to the renamed `duckduckgo-search`)
- `openpyxl` — Excel read/write
- `python-docx` — Word document read/write
- `pypdf` — PDF text extraction
- `pyyaml` — Config file parsing

## Goose Config

```yaml
extensions:
  developer:
    enabled: false
  computer_controller:
    enabled: false
  lean-developer-suite:
    enabled: true
    type: stdio
    cmd: "uv"
    args: ["run", "/path/to/lean-goose-mcp/lean_mcp.py"]
```
