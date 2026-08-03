"""Pure-function tests for kitty_docs_web.py — no network I/O.

Focused on the Track C/E fixes: query-filter pagination, tracking-param
stripping, markdown block splitting, and char-cap block boundaries.
"""

import http.server
import json
import sys
import threading
from pathlib import Path

import openpyxl
import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import kitty_docs_web as kdw


def test_filter_by_query_no_match_does_not_fabricate_data():
    result = kdw._filter_by_query(["apple", "banana", "cherry"], query="zzz-nonexistent")
    assert result.no_match is True
    assert result.items == ["apple", "banana", "cherry"]
    assert result.total_matches == 0


def test_filter_by_query_stable_sort_keeps_tie_order():
    items = ["cat dog", "dog cat", "dog only"]
    result = kdw._filter_by_query(items, query="cat dog")
    # "cat dog" and "dog cat" tie at score 2; "dog only" scores 1.
    assert result.items[:2] == ["cat dog", "dog cat"]
    assert result.items[2] == "dog only"


def test_filter_by_query_offset_and_next_offset():
    items = [f"apple item {i}" for i in range(10)]
    first = kdw._filter_by_query(items, query="apple", max_results=4, offset=0)
    assert len(first.items) == 4
    assert first.truncated is True
    assert first.next_offset == 4

    second = kdw._filter_by_query(items, query="apple", max_results=4, offset=first.next_offset)
    assert second.items == items[4:8]
    assert second.next_offset == 8


def test_strip_tracking_params_preserves_resource_query_string():
    url = "https://youtube.com/watch?v=abc123&utm_source=newsletter"
    cleaned = kdw._strip_tracking_params(url)
    assert "v=abc123" in cleaned
    assert "utm_source" not in cleaned


def test_strip_tracking_params_no_query_string_passthrough():
    assert kdw._strip_tracking_params("https://example.com/page") == "https://example.com/page"


def test_split_markdown_blocks_keeps_fenced_code_intact():
    text = "para one\n\n```\nline a\n\nline b\n```\n\npara two"
    blocks = kdw._split_markdown_blocks(text)
    assert blocks[0] == "para one"
    assert "```" in blocks[1] and "line a" in blocks[1] and "line b" in blocks[1]
    assert blocks[2] == "para two"


def test_cap_blocks_by_chars_never_splits_mid_block():
    blocks = ["a" * 10, "b" * 10, "c" * 10]
    text, n_used = kdw._cap_blocks_by_chars(blocks, cap=15)
    assert text == "a" * 10
    assert n_used == 1


def test_cap_blocks_by_chars_returns_something_when_cap_smaller_than_one_block():
    blocks = ["x" * 50]
    text, n_used = kdw._cap_blocks_by_chars(blocks, cap=10)
    assert text == "x" * 10
    assert n_used == 1


def test_success_response_omits_falsy_message_and_metadata():
    payload = json.loads(kdw.success_response("data", message="", metadata=None))
    assert "message" not in payload
    assert "metadata" not in payload
    assert payload["data"] == "data"
    assert payload["truncated"] is False


def test_error_response_scrape_hint_is_not_search_keyword_advice():
    # Track C fix: the old shared hint mapper told a *scrape* failure to
    # "Broaden search keywords" — nonsense for a page that failed to parse.
    payload = json.loads(kdw.error_response("SCRAPE_EMPTY", "no content"))
    assert "broaden search keywords" not in payload["hint"].lower()


# ---------------------------------------------------------------------------
# Tool-level fixture tests (real files / a local HTTP server — no external
# network)
# ---------------------------------------------------------------------------


@pytest.fixture
def local_server():
    """Starts a throwaway local HTTP server; the caller sets `.handler_cls`
    before making requests. Yields the base URL."""
    state = {"handler_cls": None}

    class Dispatch(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            state["handler_cls"](self).do_GET()

        def log_message(self, *a):
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Dispatch)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", state
    finally:
        server.shutdown()


class _HtmlPage:
    BODY = (
        b"<html><head><title>Test Page</title></head><body><article>"
        b"<h1>Big Title</h1><p>This is a long paragraph about apples and oranges "
        b"that trafilatura should extract cleanly as the main body content of the "
        b"page for testing purposes right here.</p>"
        b"<p>Second paragraph about bananas only, unrelated to the first topic "
        b"entirely for filtering tests.</p></article></body></html>"
    )

    def __init__(self, handler):
        self.handler = handler

    def do_GET(self):
        self.handler.send_response(200)
        self.handler.send_header("Content-Type", "text/html; charset=utf-8")
        self.handler.end_headers()
        self.handler.wfile.write(self.BODY)


class _JsonPage:
    def __init__(self, handler):
        self.handler = handler

    def do_GET(self):
        self.handler.send_response(200)
        self.handler.send_header("Content-Type", "application/json")
        self.handler.end_headers()
        self.handler.wfile.write(b'{"a": 1}')


def test_web_scrape_extracts_body_and_metadata(local_server):
    base_url, state = local_server
    state["handler_cls"] = _HtmlPage
    result = json.loads(kdw.web_scrape(f"{base_url}/page"))
    assert result["status"] == "success"
    assert "apples" in result["data"]
    assert result["metadata"]["title"]


def test_web_scrape_query_filter_isolates_matching_block(local_server):
    base_url, state = local_server
    state["handler_cls"] = _HtmlPage
    result = json.loads(kdw.web_scrape(f"{base_url}/page", query="bananas"))
    assert "bananas" in result["data"].lower()
    assert "apples" not in result["data"].lower()


def test_web_scrape_rejects_non_html_content_type_instead_of_scrape_empty(local_server):
    base_url, state = local_server
    state["handler_cls"] = _JsonPage
    result = json.loads(kdw.web_scrape(f"{base_url}/data.json"))
    assert result["status"] == "error"
    assert result["error_code"] == "SCRAPE_UNSUPPORTED_CONTENT_TYPE"


def test_excel_read_rows_caps_and_paginates(tmp_path):
    xlsx = tmp_path / "big.xlsx"
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.append(["id", "name"])
    for i in range(600):
        ws.append([i, f"row{i}"])
    wb.save(xlsx)

    first = json.loads(kdw.excel_read_rows(str(xlsx)))
    assert first["truncated"] is True
    assert len(first["data"]) == kdw.EXCEL_MAX_ROWS_DEFAULT
    assert first["metadata"]["total_rows"] == 600
    assert first["metadata"]["next_offset"] == kdw.EXCEL_MAX_ROWS_DEFAULT

    second = json.loads(kdw.excel_read_rows(str(xlsx), offset=first["metadata"]["next_offset"]))
    assert second["data"][0]["id"] == kdw.EXCEL_MAX_ROWS_DEFAULT


def test_pdf_read_text_and_outline(tmp_path):
    fitz = pytest.importorskip("fitz")
    pdf_path = tmp_path / "test.pdf"
    doc = fitz.open()
    doc.new_page().insert_text((72, 72), "Hello page one apple")
    doc.new_page().insert_text((72, 72), "Hello page two banana")
    doc.set_toc([[1, "Chapter 1", 1], [1, "Chapter 2", 2]])
    doc.save(pdf_path)
    doc.close()

    result = json.loads(kdw.pdf_read_text(str(pdf_path)))
    assert result["status"] == "success"
    assert result["metadata"]["total_pages"] == 2

    filtered = json.loads(kdw.pdf_read_text(str(pdf_path), query="banana"))
    assert filtered["metadata"]["total_matches"] == 1

    outline = json.loads(kdw.pdf_read_outline(str(pdf_path)))
    assert len(outline["data"]) == 2
