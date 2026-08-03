"""Tests for lean_web_search / lean_web_search_read_chunk — no live network.

Brave/DuckDuckGo calls are mocked at the `_brave_query`/`_ddg_query`
boundary for orchestration tests (tier selection, dual-engine fan-out,
fallback behavior) and at the `httpx.get` boundary for `_brave_query`'s own
retry/classification logic, matching the repo convention of never hitting
live network in tests.
"""

import json
import sys
import time
from pathlib import Path
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import kitty_docs_web as kdw

FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"


# ---------------------------------------------------------------------------
# Pure helpers
# ---------------------------------------------------------------------------
def test_query_guardrails_strip_fluff_and_clamp_length():
    assert kdw._apply_query_guardrails("please search for cats") == "cats"
    assert kdw._apply_query_guardrails("What is the speed of light") == "the speed of light"
    assert kdw._apply_query_guardrails('"quoted query"') == "quoted query"
    long_query = " ".join(f"word{i}" for i in range(80))
    guarded = kdw._apply_query_guardrails(long_query)
    assert len(guarded.split()) <= 50
    assert len(guarded) <= 400


def test_normalize_url_key_dedup():
    a = kdw._normalize_url_key("https://Example.com/Page?utm_source=x")
    b = kdw._normalize_url_key("http://www.example.com/Page/")
    assert a == b


def test_extract_keywords_deterministic_tie_break():
    # "apple" and "orange" both appear twice; "apple" appears first, so it
    # must win the tie under a stable sort keyed only on -count.
    text = "apple orange apple orange banana"
    keywords = kdw._extract_keywords(text, top_k=2)
    assert keywords == ["apple", "orange"]


def test_extract_keywords_filters_stopwords_and_short_words():
    text = "the a is of to it in on and this that with keyword appears here"
    keywords = kdw._extract_keywords(text, top_k=5)
    assert "the" not in keywords
    assert "is" not in keywords
    assert "keyword" in keywords


# ---------------------------------------------------------------------------
# Tier selection / orchestration (mocked at the engine-call boundary)
# ---------------------------------------------------------------------------
def _brave_item(title="Brave Result", url="https://brave-example.com/a"):
    return {"title": title, "domain": "brave-example.com", "url": url, "date": None, "snippet": "brave snippet"}


def _ddg_item(title="DDG Result", url="https://ddg-example.com/b"):
    return {"title": title, "domain": "ddg-example.com", "url": url, "date": None, "snippet": "ddg snippet"}


def test_default_count_is_5_and_normal_mode(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: [_brave_item()])
    monkeypatch.setattr(kdw, "_ddg_query", Mock(side_effect=AssertionError("DDG should not be called")))

    result = json.loads(kdw.web_search("test query"))
    assert result["status"] == "success"
    assert result["metadata"]["mode"] == "normal"
    assert result["metadata"]["count"] == 5


def test_count_le_5_uses_brave_then_fallback_only_on_failure(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")

    # Brave succeeds -> DDG never called.
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: [_brave_item()])
    ddg_mock = Mock(side_effect=AssertionError("should not be called"))
    monkeypatch.setattr(kdw, "_ddg_query", ddg_mock)
    result = json.loads(kdw.web_search("q", count=3))
    assert result["data"][0]["engine"] == "brave"

    # Brave fails (network) -> DDG is used as fallback.
    monkeypatch.setattr(kdw, "_brave_query", Mock(side_effect=kdw._BraveFailure("network", "boom")))
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [_ddg_item()])
    result = json.loads(kdw.web_search("q", count=3))
    assert result["data"][0]["engine"] == "duckduckgo"
    assert result["metadata"]["engines"]["brave"] == "network"


def test_count_6_to_10_queries_both_engines_concurrently(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    brave_mock = Mock(return_value=[_brave_item()])
    ddg_mock = Mock(return_value=[_ddg_item()])
    monkeypatch.setattr(kdw, "_brave_query", brave_mock)
    monkeypatch.setattr(kdw, "_ddg_query", ddg_mock)

    result = json.loads(kdw.web_search("q", count=8))
    assert result["metadata"]["mode"] == "expanded"
    brave_mock.assert_called_once()
    ddg_mock.assert_called_once()
    engines = {item["engine"] for item in result["data"]}
    assert engines == {"brave", "duckduckgo"}


def test_count_above_10_writes_offload_and_returns_keyword_index(monkeypatch, tmp_path):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw, "SEARCH_STORE_DIR", tmp_path / "search-offload")
    brave_items = [_brave_item(title=f"Brave item {i}", url=f"https://brave-example.com/{i}") for i in range(15)]
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: brave_items)
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [])

    result = json.loads(kdw.web_search("q", count=15))
    assert result["metadata"]["mode"] == "expansive"
    assert "search_id" in result["metadata"]
    for item in result["data"]:
        assert "url" not in item
        assert "snippet" not in item
        assert "keywords" in item
        assert "id" in item

    offload_files = list((tmp_path / "search-offload").glob("search-*.json"))
    assert len(offload_files) == 1
    stored = json.loads(offload_files[0].read_text(encoding="utf-8"))
    assert len(stored["results"]) == 15
    assert all("url" in r and "snippet" in r for r in stored["results"])


def test_dual_engine_search_falls_back_to_ddg_only_when_no_brave_key(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "")
    brave_mock = Mock(side_effect=AssertionError("should not be called without a key"))
    monkeypatch.setattr(kdw, "_brave_query", brave_mock)
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [_ddg_item()])

    result = json.loads(kdw.web_search("q", count=8))
    brave_mock.assert_not_called()
    assert result["metadata"]["engines"]["brave"] == "not_configured"
    assert all(item["engine"] == "duckduckgo" for item in result["data"])


# ---------------------------------------------------------------------------
# read_chunk
# ---------------------------------------------------------------------------
def test_read_chunk_returns_full_detail_for_requested_ids(monkeypatch, tmp_path):
    monkeypatch.setattr(kdw, "SEARCH_STORE_DIR", tmp_path / "search-offload")
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    brave_items = [_brave_item(title=f"Item {i}", url=f"https://brave-example.com/{i}") for i in range(15)]
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: brave_items)
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [])

    search_result = json.loads(kdw.web_search("q", count=15))
    search_id = search_result["metadata"]["search_id"]

    chunk = json.loads(kdw.web_search_read_chunk(search_id, [1, 3]))
    assert chunk["status"] == "success"
    ids = {item["id"] for item in chunk["data"]}
    assert ids == {1, 3}
    assert all("url" in item and "snippet" in item for item in chunk["data"])


def test_read_chunk_unknown_search_id_is_clean_error():
    result = json.loads(kdw.web_search_read_chunk("nonexistent-id-1234", [1]))
    assert result["status"] == "error"
    assert result["error_code"] == "SEARCH_ID_NOT_FOUND"


def test_read_chunk_rejects_path_traversal_search_id():
    result = json.loads(kdw.web_search_read_chunk("../../etc/passwd", [1]))
    assert result["status"] == "error"
    assert result["error_code"] == "SEARCH_ID_NOT_FOUND"


def test_prune_old_offloads_keeps_only_max_recent_files(monkeypatch, tmp_path):
    store = tmp_path / "search-offload"
    monkeypatch.setattr(kdw, "SEARCH_STORE_DIR", store)
    store.mkdir(parents=True)
    extra = 5
    total = kdw.MAX_OFFLOAD_FILES + extra
    for i in range(total):
        f = store / f"search-{i:04d}.json"
        f.write_text("{}")
        # Ensure distinct, increasing mtimes regardless of filesystem mtime granularity.
        mtime = time.time() + i
        import os

        os.utime(f, (mtime, mtime))

    kdw._prune_old_offloads()
    remaining = list(store.glob("search-*.json"))
    assert len(remaining) == kdw.MAX_OFFLOAD_FILES - 1


# ---------------------------------------------------------------------------
# Brave client: retry/backoff/classification (mocked at the httpx.get boundary)
# ---------------------------------------------------------------------------
def _brave_response(status_code, json_body=None, headers=None, text=""):
    resp = Mock()
    resp.status_code = status_code
    resp.is_success = 200 <= status_code < 300
    resp.headers = headers or {}
    resp.text = text
    if json_body is not None:
        resp.json = Mock(return_value=json_body)
    return resp


def test_brave_429_retries_then_succeeds(monkeypatch):
    monkeypatch.setattr(kdw.time, "sleep", lambda *_: None)
    ok_payload = {
        "grounding": {"generic": [{"title": "T", "url": "https://x.com/a", "snippets": ["s"]}]},
        "sources": [],
    }
    responses = [_brave_response(429, headers={}), _brave_response(200, json_body=ok_payload)]
    monkeypatch.setattr(kdw.httpx, "get", Mock(side_effect=responses))

    results = kdw._brave_query("q", 5, "en", None, "US")
    assert len(results) == 1
    assert results[0]["title"] == "T"


def test_brave_429_exhausted_falls_back_to_ddg(monkeypatch):
    monkeypatch.setattr(kdw.time, "sleep", lambda *_: None)
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw.httpx, "get", Mock(return_value=_brave_response(429, headers={})))
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [_ddg_item()])

    result = json.loads(kdw.web_search("q", count=3))
    assert result["data"][0]["engine"] == "duckduckgo"
    assert result["metadata"]["engines"]["brave"] == "rate_limit_exhausted"


def test_brave_invalid_query_does_not_fall_back(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw.httpx, "get", Mock(return_value=_brave_response(422, text="bad query")))
    ddg_mock = Mock(side_effect=AssertionError("should not be called"))
    monkeypatch.setattr(kdw, "_ddg_query", ddg_mock)

    result = json.loads(kdw.web_search("q", count=3))
    assert result["status"] == "error"
    assert result["error_code"] == "INVALID_QUERY"
    ddg_mock.assert_not_called()


def test_brave_invalid_query_in_dual_engine_mode_does_not_abort_ddg(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw.httpx, "get", Mock(return_value=_brave_response(422, text="bad query")))
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [_ddg_item()])

    result = json.loads(kdw.web_search("q", count=8))
    assert result["status"] == "success"
    assert result["metadata"]["engines"]["brave"] == "invalid_query"
    assert all(item["engine"] == "duckduckgo" for item in result["data"])


# ---------------------------------------------------------------------------
# Brave response-shape parsing
#
# Recorded from a live `GET /res/v1/llm/context` call: `sources` is a **dict
# keyed by URL**, and each source's date lives in an `age` list, not a `date`
# key. The parser previously assumed a list of `{"url": ...}` objects, so
# iterating yielded plain strings and raised AttributeError — which escaped
# `_brave_query` entirely and took the whole tool down instead of falling
# back to DuckDuckGo.
# ---------------------------------------------------------------------------
LIVE_BRAVE_PAYLOAD = {
    "grounding": {
        "generic": [
            {
                "url": "https://v2.tauri.app/plugin/global-shortcut/",
                "title": "Global Shortcut",
                "snippets": ["Install the global-shortcut plugin to get started."],
            }
        ],
        "map": [],
    },
    "sources": {
        "https://v2.tauri.app/plugin/global-shortcut/": {
            "title": "Global Shortcut",
            "hostname": "v2.tauri.app",
            "age": [
                "Saturday, February 22, 2025",
                "2025-02-22",
                "526 days ago",
                "2025-02-22T00:00:00Z",
            ],
            "snippet": "Install the global-shortcut plugin to get started.",
        }
    },
}


def test_parse_brave_results_handles_dict_keyed_sources():
    items = kdw._parse_brave_results(LIVE_BRAVE_PAYLOAD)
    assert len(items) == 1
    assert items[0]["url"] == "https://v2.tauri.app/plugin/global-shortcut/"
    assert items[0]["domain"] == "v2.tauri.app"
    # ISO rendering preferred over "Saturday, February 22, 2025" / "526 days ago".
    assert items[0]["date"] == "2025-02-22"


def test_parse_brave_results_still_handles_list_sources():
    payload = {
        "grounding": {"generic": [{"url": "https://x.com/a", "title": "T", "snippets": ["s"]}]},
        "sources": [{"url": "https://x.com/a", "hostname": "x.com", "age": "2025-01-01"}],
    }
    items = kdw._parse_brave_results(payload)
    assert items[0]["date"] == "2025-01-01"


def test_source_date_prefers_iso_then_first_entry():
    assert kdw._source_date({"age": ["3 days ago", "2024-06-01"]}) == "2024-06-01"
    assert kdw._source_date({"age": ["3 days ago"]}) == "3 days ago"
    assert kdw._source_date({"age": []}) is None
    assert kdw._source_date({}) is None


def test_unexpected_brave_shape_becomes_brave_failure_not_a_raw_exception(monkeypatch):
    # `sources: 12` is nonsense of a kind no defensive branch anticipates; the
    # point is that *whatever* breaks in parsing surfaces as _BraveFailure.
    monkeypatch.setattr(kdw, "_parse_brave_results", Mock(side_effect=TypeError("boom")))
    monkeypatch.setattr(
        kdw.httpx, "get", Mock(return_value=_brave_response(200, json_body={"sources": 12}))
    )
    try:
        kdw._brave_query("q", 5, "en", None, "US")
    except kdw._BraveFailure as e:
        assert e.kind == "api"
        assert "TypeError" in e.detail
    else:
        raise AssertionError("expected _BraveFailure")


def test_search_survives_a_sources_list_of_bare_url_strings(monkeypatch):
    """The user-visible regression, end to end: with a Brave key configured,
    every search died with "'str' object has no attribute 'get'". A `sources`
    collection whose members aren't objects is now simply ignored — grounding
    carries the results, sources are only supplementary."""
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    odd = dict(LIVE_BRAVE_PAYLOAD, sources=["https://v2.tauri.app/plugin/global-shortcut/"])
    monkeypatch.setattr(kdw.httpx, "get", Mock(return_value=_brave_response(200, json_body=odd)))
    monkeypatch.setattr(kdw, "_ddg_query", Mock(side_effect=AssertionError("should not be needed")))

    result = json.loads(kdw.web_search("q", count=3))
    assert result["status"] == "success"
    assert result["data"][0]["engine"] == "brave"


def test_unparseable_brave_payload_falls_back_to_ddg_instead_of_failing_the_tool(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw, "_parse_brave_results", Mock(side_effect=TypeError("boom")))
    monkeypatch.setattr(
        kdw.httpx, "get", Mock(return_value=_brave_response(200, json_body={"grounding": {}}))
    )
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [_ddg_item()])

    result = json.loads(kdw.web_search("q", count=3))
    assert result["status"] == "success"
    assert result["data"][0]["engine"] == "duckduckgo"
    assert result["metadata"]["engines"]["brave"] == "api"


def test_ddg_failure_in_normal_mode_reports_all_engines_failed(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "")
    monkeypatch.setattr(kdw, "_ddg_query", Mock(side_effect=RuntimeError("network down")))

    result = json.loads(kdw.web_search("q", count=3))
    assert result["status"] == "error"
    assert result["error_code"] == "ALL_ENGINES_FAILED"
    assert "network down" in result["detail"]


def test_genuinely_empty_results_still_report_no_results(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "")
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [])

    result = json.loads(kdw.web_search("q", count=3))
    assert result["error_code"] == "NO_RESULTS"


# ---------------------------------------------------------------------------
# Inline/full snippet split, always-offload, byte-budget downgrade
#
# Motivated by a live measurement: Brave's `/llm/context` grounding snippets
# run ~4.5K chars median (page extracts, not blurbs) versus its own
# `sources[].snippet` at ~240 chars. A single count=10 search returned ~32K
# chars inline before this change. tests/fixtures/brave_llm_context.json is a
# real recorded response (no key/token material — grep-verified) exercising
# the actual dict-keyed `sources` shape.
# ---------------------------------------------------------------------------
def _load_brave_fixture():
    return json.loads((FIXTURES_DIR / "brave_llm_context.json").read_text(encoding="utf-8"))


def test_live_fixture_search_stays_under_the_inline_budget(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    fixture = _load_brave_fixture()
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: kdw._parse_brave_results(fixture))
    monkeypatch.setattr(kdw, "_ddg_query", Mock(side_effect=AssertionError("brave should win")))

    result_json = kdw.web_search("rust async runtime comparison", count=5)
    result = json.loads(result_json)
    assert result["status"] == "success"
    assert len(result_json) <= kdw.INLINE_RESPONSE_MAX_CHARS
    # The regression this whole change exists for: unsplit grounding text
    # alone summed to ~32K chars for a 10-result fetch of this same fixture.
    unsplit_chars = sum(
        len(" ".join(e.get("snippets") or [])) for e in fixture["grounding"]["generic"]
    )
    assert unsplit_chars > kdw.INLINE_RESPONSE_MAX_CHARS


def test_inline_items_never_carry_snippet_full_and_respect_the_per_item_cap(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    fixture = _load_brave_fixture()
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: kdw._parse_brave_results(fixture))
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [])

    result = json.loads(kdw.web_search("rust async runtime comparison", count=5))
    for item in result["data"]:
        assert "snippet_full" not in item
        assert len(item["snippet"]) <= kdw.INLINE_SNIPPET_MAX_CHARS


def test_normal_mode_returns_search_id_and_read_chunk_gets_full_text(monkeypatch, tmp_path):
    monkeypatch.setattr(kdw, "SEARCH_STORE_DIR", tmp_path / "search-offload")
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    long_full = "x" * 5000
    item = _brave_item()
    item["snippet"] = "short"
    item["snippet_full"] = long_full
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: [item])
    monkeypatch.setattr(kdw, "_ddg_query", Mock(side_effect=AssertionError("should not be called")))

    result = json.loads(kdw.web_search("q", count=3))
    assert result["metadata"]["mode"] == "normal"
    search_id = result["metadata"]["search_id"]
    assert search_id

    chunk = json.loads(kdw.web_search_read_chunk(search_id, [1]))
    assert chunk["status"] == "success"
    assert chunk["data"][0]["snippet"] == long_full


def test_empty_source_snippet_falls_back_to_truncated_grounding_text():
    payload = {
        "grounding": {
            "generic": [
                {
                    "url": "https://x.com/a",
                    "title": "T",
                    "snippets": ["full page extract " * 50],
                }
            ]
        },
        "sources": {"https://x.com/a": {"hostname": "x.com", "snippet": ""}},
    }
    items = kdw._parse_brave_results(payload)
    assert items[0]["snippet"] != ""
    assert items[0]["snippet"] == items[0]["snippet_full"][: kdw.INLINE_SNIPPET_MAX_CHARS]


def test_oversized_results_downgrade_to_index_with_metadata(monkeypatch):
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    # Even after the per-item cap, enough results can still exceed the
    # response-level budget — simulate that directly rather than relying on
    # a specific result count.
    monkeypatch.setattr(kdw, "INLINE_RESPONSE_MAX_CHARS", 50)
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: [_brave_item()])
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [])

    result = json.loads(kdw.web_search("q", count=3))
    assert result["metadata"]["mode"] == "normal"
    assert result["metadata"]["downgraded_to_index"] is True
    assert result["metadata"]["inline_chars"] > 50
    assert "keywords" in result["data"][0]
    assert "snippet" not in result["data"][0]


def test_build_index_keywords_drawn_from_full_text_not_truncated_inline_snippet():
    results = [
        {
            "id": 1,
            "title": "T",
            "domain": "x.com",
            "engine": "brave",
            "snippet": "short",
            "snippet_full": "short " + "uniquefulltextonlyword " * 3,
        }
    ]
    manifest = kdw._build_index(results)
    assert "uniquefulltextonlyword" in manifest[0]["keywords"]


def test_read_chunk_respects_char_ceiling_and_reports_truncation(monkeypatch, tmp_path):
    monkeypatch.setattr(kdw, "SEARCH_STORE_DIR", tmp_path / "search-offload")
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw, "READ_CHUNK_MAX_CHARS", 500)
    big_items = []
    for i in range(3):
        it = _brave_item(title=f"Item {i}", url=f"https://brave-example.com/{i}")
        it["snippet_full"] = "y" * 400
        big_items.append(it)
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: big_items)
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [])

    search_result = json.loads(kdw.web_search("q", count=3))
    search_id = search_result["metadata"]["search_id"]

    chunk = json.loads(kdw.web_search_read_chunk(search_id, [1, 2, 3]))
    assert chunk["truncated"] is True
    assert len(chunk["data"]) < 3
    assert chunk["metadata"]["ids_returned"] == [item["id"] for item in chunk["data"]]


def test_read_chunk_always_returns_at_least_one_item_even_over_ceiling(monkeypatch, tmp_path):
    """A single result whose snippet_full alone exceeds READ_CHUNK_MAX_CHARS
    must still come back — the ceiling should never starve a caller down to
    zero results."""
    monkeypatch.setattr(kdw, "SEARCH_STORE_DIR", tmp_path / "search-offload")
    monkeypatch.setattr(kdw, "BRAVE_API_KEY", "test-key")
    monkeypatch.setattr(kdw, "READ_CHUNK_MAX_CHARS", 10)
    item = _brave_item()
    item["snippet_full"] = "z" * 1000
    monkeypatch.setattr(kdw, "_brave_query", lambda *a, **k: [item])
    monkeypatch.setattr(kdw, "_ddg_query", lambda *a, **k: [])

    search_result = json.loads(kdw.web_search("q", count=3))
    search_id = search_result["metadata"]["search_id"]

    chunk = json.loads(kdw.web_search_read_chunk(search_id, [1]))
    assert chunk["status"] == "success"
    assert len(chunk["data"]) == 1
