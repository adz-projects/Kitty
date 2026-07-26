import json
import os
import re
from typing import Any, Dict, List, Optional
from urllib.parse import urlparse
import httpx
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("Brave Search MCP Server")

# ---------------------------------------------------------------------------
# Module-level HTTP Client (Connection Pooling & Keep-Alive)
# ---------------------------------------------------------------------------
_http_client: Optional[httpx.AsyncClient] = None


def get_http_client() -> httpx.AsyncClient:
    """Reuses a single AsyncClient instance across tool invocations to enable TCP/TLS pooling."""
    global _http_client
    if _http_client is None or _http_client.is_closed:
        _http_client = httpx.AsyncClient(timeout=10.0)
    return _http_client


# ---------------------------------------------------------------------------
# Pre-compiled Regex Patterns (Eliminates Re-compilation Overhead)
# ---------------------------------------------------------------------------
RE_QUERY_FLUFF = re.compile(
    r"^(please\s+)?(search\s+for|find\s+me|look\s+up|what\s+is|who\s+is|where\s+is|can\s+you\s+search\s+for)\s+",
    re.IGNORECASE,
)
RE_SCHEME_WWW = re.compile(r"^(https?://)?(www\.)?", re.IGNORECASE)


# ---------------------------------------------------------------------------
# Helper Functions (Optimized for Pure Python Performance)
# ---------------------------------------------------------------------------
def _error_response(
    code: str,
    message: str,
    detail: Optional[str] = None,
    hint: Optional[str] = None,
) -> str:
    """Returns a standardized JSON error payload with actionable recovery hints."""
    payload: Dict[str, Any] = {
        "status": "error",
        "error_code": code,
        "message": message,
    }
    if detail:
        payload["detail"] = detail
    if hint:
        payload["hint"] = hint
    return json.dumps(payload, indent=2, ensure_ascii=False)


def _clean_url(url: str) -> str:
    """Fast URL parameter cleaning using native string slicing instead of regex passes."""
    if not url:
        return ""
    return url.split("?")[0].split("#")[0].rstrip("/")


def _normalize_url_key(url: str) -> str:
    """Normalizes URLs for O(1) dictionary key lookup."""
    if not url:
        return ""
    clean = _clean_url(url).lower()
    return RE_SCHEME_WWW.sub("", clean)


def _apply_query_guardrails(query: str) -> str:
    """Strips conversational fluff and clamps query bounds."""
    if not query:
        return ""
    cleaned = query.strip()
    cleaned = RE_QUERY_FLUFF.sub("", cleaned).strip('\'"')
    words = cleaned.split()
    if len(words) > 50:
        cleaned = " ".join(words[:50])
    if len(cleaned) > 400:
        cleaned = cleaned[:400].rstrip()
    return cleaned


def _process_item(
    item: Any,
    res_type: str,
    sources_map: Dict[str, dict],
    title_fallback: str = "Untitled",
) -> Optional[dict]:
    """Single-pass processor for generic web, POI, and map search result items."""
    if not isinstance(item, dict):
        return None

    raw_url = item.get("url", "")
    clean_url = _clean_url(raw_url)
    norm_key = _normalize_url_key(raw_url)
    meta = sources_map.get(norm_key, {})

    # Extract publication date from sources metadata
    age_info = meta.get("age")
    date_str = None
    if isinstance(age_info, list) and age_info:
        date_str = age_info[1] if len(age_info) > 1 else age_info[0]

    hostname = meta.get("hostname") or (urlparse(clean_url).netloc if clean_url else "")
    raw_snippets = item.get("snippets", [])
    snippets = [s.strip() for s in raw_snippets if isinstance(s, str) and s.strip()]

    title = item.get("title") or item.get("name") or title_fallback

    return {
        "type": res_type,
        "title": title,
        "domain": hostname,
        "url": clean_url,
        "date": date_str,
        "snippets": snippets,
    }


def format_llm_context_json(data: dict) -> str:
    """Transforms Brave's grounding & sources API response into a pure JSON schema."""
    grounding = data.get("grounding", {})
    raw_sources = data.get("sources", {})

    # Pre-compute normalized lookup dictionary in a single list comprehension
    sources_map = {
        _normalize_url_key(url): meta
        for url, meta in raw_sources.items()
        if isinstance(meta, dict) and url
    }

    results = []

    # 1. Process main web results
    for item in grounding.get("generic", []):
        processed = _process_item(item, "web", sources_map)
        if processed:
            results.append(processed)

    # 2. Process Point of Interest (POI) local result
    poi = grounding.get("poi")
    if poi:
        processed = _process_item(poi, "poi", sources_map, title_fallback="Local Result")
        if processed:
            results.append(processed)

    # 3. Process map results
    for map_item in grounding.get("map", []):
        processed = _process_item(map_item, "map", sources_map, title_fallback="Map Result")
        if processed:
            results.append(processed)

    payload = {
        "status": "success",
        "total_results": len(results),
        "results": results,
    }

    return json.dumps(payload, indent=2, ensure_ascii=False)


# ---------------------------------------------------------------------------
# Tool Entry Point
# ---------------------------------------------------------------------------
@mcp.tool()
async def brave_mcp_search(
    q: str,
    count: int = 5,
    search_lang: str = "en",
    freshness: str | None = None,
    country: str = "US",
) -> str:
    """Search the web using the Brave Search LLM Context API.

    Args:
        q: The search query string (max 400 characters, 50 words).
        count: Number of web results evaluated (1 to 50, default 5).
        search_lang: 2-letter language code (default 'en').
        freshness: Filter results by age/date range. Options:
            - 'pd': Past 24 hours (past day)
            - 'pw': Past 7 days (past week)
            - 'pm': Past 31 days (past month)
            - 'py': Past 365 days (past year)
            - 'YYYY-MM-DDtoYYYY-MM-DD': Custom date range (e.g. '2025-01-01to2025-06-30')
        country: 2-letter country code for regional results (default 'US').
    """
    api_key = os.environ.get("BRAVE_API_KEY")
    if not api_key:
        return _error_response(
            "CONFIG_ERROR",
            "BRAVE_API_KEY environment variable is not set.",
            hint="Set the BRAVE_API_KEY environment variable before running the MCP server.",
        )

    guarded_q = _apply_query_guardrails(q)
    if not guarded_q:
        return _error_response(
            "EMPTY_QUERY",
            "The search query string was empty or contained only whitespace.",
            hint="Provide a non-empty search query parameter 'q'.",
        )

    url = "https://api.search.brave.com/res/v1/llm/context"

    headers = {
        "Accept": "application/json",
        "Accept-Encoding": "gzip",
        "X-Subscription-Token": api_key,
    }

    params = {
        "q": guarded_q,
        "count": min(max(count, 1), 50),
        "search_lang": search_lang,
        "country": country,
    }

    if freshness:
        params["freshness"] = freshness

    client = get_http_client()

    try:
        response = await client.get(url, headers=headers, params=params)
        response.raise_for_status()
        return format_llm_context_json(response.json())

    except httpx.HTTPStatusError as e:
        status = e.response.status_code
        detail = e.response.text

        if status in (400, 422):
            return _error_response(
                "INVALID_QUERY",
                f"Brave Search API rejected query parameters (HTTP {status}).",
                detail=detail,
                hint="Simplify query parameter 'q' or check freshness/country values.",
            )
        elif status in (401, 403):
            return _error_response(
                "AUTH_ERROR",
                f"Authentication failed (HTTP {status}).",
                detail=detail,
                hint="Verify your BRAVE_API_KEY is valid and active.",
            )
        elif status == 429:
            return _error_response(
                "RATE_LIMIT",
                "Brave Search API rate limit exceeded (HTTP 429).",
                detail=detail,
                hint="Pause briefly before retrying search requests.",
            )
        else:
            return _error_response(
                "API_ERROR",
                f"Brave Search API returned HTTP {status}.",
                detail=detail,
                hint="Check request formatting or retry shortly.",
            )

    except httpx.RequestError as e:
        return _error_response(
            "NETWORK_ERROR",
            "Failed to communicate with Brave Search API.",
            detail=str(e),
            hint="Check host internet connectivity.",
        )


def main() -> None:
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()