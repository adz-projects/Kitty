import os
import httpx
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("Brave Search MCP Server")


def format_llm_context(data: dict) -> str:
    """Postprocess Brave Search LLM Context API response into clean, token-efficient Markdown."""
    grounding = data.get("grounding", {})
    sources = data.get("sources", {})

    generic_results = grounding.get("generic", [])
    poi = grounding.get("poi")
    map_results = grounding.get("map", [])

    if not generic_results and not poi and not map_results:
        return "No relevant web context found for this query."

    output_sections = []

    # 1. Format main web search results
    for idx, item in enumerate(generic_results, 1):
        url = item.get("url", "")
        title = item.get("title", "Untitled")
        snippets = item.get("snippets", [])

        # Merge metadata from top-level sources dict
        source_meta = sources.get(url, {})
        hostname = source_meta.get("hostname", "")
        age_info = source_meta.get("age")
        date_str = (
            f" | Date: {age_info[0]}"
            if age_info and isinstance(age_info, list) and age_info
            else ""
        )

        header = f"### [{idx}] {title}"
        if hostname:
            header += f" ({hostname})"

        lines = [header, f"**URL:** {url}{date_str}", "**Extracted Content:**"]
        for snippet in snippets:
            lines.append(f"- {snippet.strip()}")

        output_sections.append("\n".join(lines))

    # 2. Format POI (Point of Interest) if local recall is active
    if poi:
        poi_name = poi.get("name", "Local Result")
        poi_url = poi.get("url", "")
        poi_snippets = poi.get("snippets", [])

        poi_lines = [
            f"### [Local Result] {poi_name}",
            f"**URL:** {poi_url}" if poi_url else "",
            "**Details:**",
        ]
        for snippet in poi_snippets:
            poi_lines.append(f"- {snippet.strip()}")

        output_sections.append("\n".join(poi_lines))

    return "\n\n---\n\n".join(output_sections)


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
        return "Error: BRAVE_API_KEY environment variable is not set."

    url = "https://api.search.brave.com/res/v1/llm/context"

    headers = {
        "Accept": "application/json",
        "Accept-Encoding": "gzip",
        "X-Subscription-Token": api_key,
    }

    clamped_count = min(max(count, 1), 50)

    params = {
        "q": q,
        "count": clamped_count,
        "search_lang": search_lang,
        "country": country,
    }

    if freshness:
        params["freshness"] = freshness

    async with httpx.AsyncClient() as client:
        try:
            response = await client.get(
                url, headers=headers, params=params, timeout=10.0
            )
            response.raise_for_status()
            data = response.json()

            return format_llm_context(data)

        except httpx.HTTPStatusError as e:
            return f"HTTP error {e.response.status_code}: {e.response.text}"
        except httpx.RequestError as e:
            return f"Network request failed: {str(e)}"


def main() -> None:
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()