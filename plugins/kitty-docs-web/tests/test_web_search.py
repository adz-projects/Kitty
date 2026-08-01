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
    ok_payload = {"grounding": {"generic": [{"title": "T", "url": "https://x.com/a", "snippets": ["s"]}]}, "sources": []}
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
