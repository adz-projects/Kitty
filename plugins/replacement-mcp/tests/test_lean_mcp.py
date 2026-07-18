"""Unit tests for lean_mcp.py's pure helpers and config loading.

Expanded from the old python_smoke_test.py stub (which had no real
assertions — just a sketch of calls against an unimplemented transport).
These exercise what's testable without a live MCP client: the private
text-processing helpers, error formatting, the scratchpad file round-trip,
and that tool_prompts.yaml actually has the shape the module expects.
"""

import json

import pytest

import lean_mcp


def test_strip_ansi_removes_escape_codes():
    colored = "\x1b[31mred text\x1b[0m plain"
    assert lean_mcp._strip_ansi(colored) == "red text plain"


def test_strip_ansi_leaves_plain_text_untouched():
    assert lean_mcp._strip_ansi("nothing to strip") == "nothing to strip"


def test_truncate_text_terse_cuts_at_terse_max():
    text = "x" * 100
    truncated, was_truncated = lean_mcp._truncate_text(text, "terse", terse_max=20, normal_max=80)
    assert was_truncated is True
    assert truncated == "x" * 20


def test_truncate_text_normal_cuts_at_normal_max():
    text = "x" * 100
    truncated, was_truncated = lean_mcp._truncate_text(text, "normal", terse_max=20, normal_max=80)
    assert was_truncated is True
    assert truncated == "x" * 80


def test_truncate_text_under_limit_is_unchanged():
    text = "short"
    truncated, was_truncated = lean_mcp._truncate_text(text, "terse", terse_max=20, normal_max=80)
    assert was_truncated is False
    assert truncated == text


def test_word_count_counts_whitespace_separated_words():
    assert lean_mcp._word_count("one two three") == 3
    assert lean_mcp._word_count("") == 0
    assert lean_mcp._word_count("   spaced   out  ") == 2


def test_error_produces_parseable_json_payload():
    result = lean_mcp.error("SOME_CODE", "a message", detail="more info", hint="try this")
    assert result.startswith("[ERR:SOME_CODE] ")
    payload = json.loads(result[len("[ERR:SOME_CODE] "):])
    assert payload == {
        "error": True,
        "code": "SOME_CODE",
        "message": "a message",
        "detail": "more info",
        "hint": "try this",
    }


def test_error_omits_absent_optional_fields():
    result = lean_mcp.error("CODE", "message only")
    payload = json.loads(result[len("[ERR:CODE] "):])
    assert "detail" not in payload
    assert "hint" not in payload


def test_scratchpad_round_trips_through_disk(tmp_path, monkeypatch):
    scratch_file = tmp_path / "scratchpad.json"
    monkeypatch.setattr(lean_mcp, "SCRATCH_FILE", scratch_file)

    assert lean_mcp._load_scratchpad() == {}

    lean_mcp._save_scratchpad({"key": "value"})
    assert scratch_file.exists()
    assert lean_mcp._load_scratchpad() == {"key": "value"}


def test_scratchpad_load_missing_file_returns_empty_dict(tmp_path, monkeypatch):
    monkeypatch.setattr(lean_mcp, "SCRATCH_FILE", tmp_path / "does-not-exist.json")
    assert lean_mcp._load_scratchpad() == {}


@pytest.mark.parametrize(
    "tool",
    [
        "shell",
        "file_editor",
        "analyze_workspace",
        "fallback_web_search",
        "web_scrape",
        "excel_manager",
        "word_manager",
        "pdf_manager",
        "cache_manager",
        "scratchpad",
    ],
)
def test_tool_prompts_yaml_has_description_for_every_registered_tool(tool):
    # Every @mcp.tool(...) registration in lean_mcp.py reads its description
    # from PROMPTS[tool]["description"] at import time — a missing entry
    # would crash the whole module on import, not just that one tool.
    assert tool in lean_mcp.PROMPTS
    assert "description" in lean_mcp.PROMPTS[tool]
    assert lean_mcp.PROMPTS[tool]["description"].strip() != ""


def test_server_instructions_present():
    assert lean_mcp.PROMPTS.get("server_instructions", "").strip() != ""
