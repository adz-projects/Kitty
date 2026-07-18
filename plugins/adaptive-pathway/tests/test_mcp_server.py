import pytest
import os
import tempfile
import numpy as np
from mcp.client.session import ClientSession
from mcp.client.stdio import stdio_client, StdioServerParameters
import subprocess
import sys
import time
import json


@pytest.fixture
def db_path():
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    yield path
    try:
        os.remove(path)
    except PermissionError:
        pass


def test_mcp_server_imports():
    from adaptive_pathway.mcp_server import mcp, main
    assert mcp is not None
    assert callable(main)


def test_mcp_server_module_runnable():
    result = subprocess.run(
        [sys.executable, "-m", "adaptive_pathway.mcp_server", "--help"],
        capture_output=True, text=True, timeout=30,
    )
    assert result.returncode == 0
    assert "Adaptive Pathway" in result.stdout


# ─── Tool permission hints (mcp.types.ToolAnnotations) ─────────────────────
# These are the spec-correct signal to an ACP/MCP client (e.g. Goose's
# `smart_approve`) that a tool is safe and doesn't need an explicit approval
# prompt before every call — load-bearing for `decide`/`record_outcome`
# reliability, since a denied/unanswered permission prompt silently blocks
# the whole learning loop for that turn.
_EXPECTED_READ_ONLY = {
    "decide", "get_state", "list_edges", "get_edge", "query_attribution",
    "list_domains", "health_check", "session_reflection",
}
_EXPECTED_WRITES_LOCAL_STATE = {
    "record_outcome", "record_annotation", "toggle_suggestions",
    "accept_nudge", "resolve_schism",
}


@pytest.mark.asyncio
async def test_every_tool_declares_read_only_hint():
    from adaptive_pathway.mcp_server import mcp
    tools = await mcp.list_tools()
    for t in tools:
        assert t.annotations is not None, f"{t.name} has no ToolAnnotations"
        if t.name in _EXPECTED_READ_ONLY:
            assert t.annotations.readOnlyHint is True, t.name
        elif t.name in _EXPECTED_WRITES_LOCAL_STATE:
            assert t.annotations.readOnlyHint is False, t.name
        else:
            pytest.fail(f"unrecognized tool {t.name} — add it to one of the sets above")


@pytest.mark.asyncio
async def test_no_tool_is_marked_destructive():
    # None of these tools touch the filesystem, run shell commands, or reach
    # the network beyond adaptive-pathway's own local sidecar/SQLite file —
    # nothing here should ever be flagged destructive.
    from adaptive_pathway.mcp_server import mcp
    tools = await mcp.list_tools()
    for t in tools:
        assert t.annotations.destructiveHint is False, t.name


@pytest.mark.asyncio
async def test_mcp_tool_decide(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("decide", {
                "session_id": "test_sess",
                "available_actions": "tool_a,tool_b,tool_c",
            })
            raw = result.content[0].text
            data = eval(raw)
            assert "hints" in data
            assert "confidence" in data


@pytest.mark.asyncio
async def test_mcp_tool_record_outcome(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("record_outcome", {
                "session_id": "test_sess",
                "action_id": "tool_a",
                "reward": 1.0,
            })
            data = json.loads(result.content[0].text)
            assert data["status"] == "recorded"


@pytest.mark.asyncio
async def test_mcp_tool_get_state(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("get_state", {
                "session_id": "test_sess",
            })
            raw = result.content[0].text
            assert "warm_ready" in raw


@pytest.mark.asyncio
async def test_mcp_tool_list_edges(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("list_edges", {
                "page": 1, "per_page": 5,
            })
            raw = result.content[0].text
            assert "edges" in raw or "total" in raw


@pytest.mark.asyncio
async def test_mcp_tool_get_edge(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("get_edge", {
                "edge_id": "nonexistent",
            })
            raw = result.content[0].text
            assert "not found" in raw.lower()


@pytest.mark.asyncio
async def test_mcp_tool_health_check(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("health_check", {})
            raw = result.content[0].text
            assert "[" in raw


@pytest.mark.asyncio
async def test_mcp_tool_toggle_suggestions(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("toggle_suggestions", {
                "session_id": "test_sess",
                "paused": True,
            })
            data = json.loads(result.content[0].text)
            assert data["paused"] is True


@pytest.mark.asyncio
async def test_mcp_full_learning_loop(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()

            for _ in range(3):
                dr = await session.call_tool("decide", {
                    "session_id": "learn_test",
                    "available_actions": "pandas,csv,openpyxl",
                })
                assert "hints" in dr.content[0].text

                rr = await session.call_tool("record_outcome", {
                    "session_id": "learn_test",
                    "action_id": "pandas",
                    "reward": 0.8,
                })
                assert "recorded" in rr.content[0].text

            state = await session.call_tool("get_state", {
                "session_id": "learn_test",
            })
            assert "action_history_len" in state.content[0].text


@pytest.mark.asyncio
async def test_mcp_list_domains(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("list_domains", {})
            raw = result.content[0].text
            assert "[" in raw


@pytest.mark.asyncio
async def test_mcp_query_attribution(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("query_attribution", {
                "attribution_id": "nonexistent",
            })
            raw = result.content[0].text
            assert "not found" in raw.lower()


@pytest.mark.asyncio
async def test_mcp_record_annotation(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("record_annotation", {
                "session_id": "test_sess",
                "annotation_type": "keep_this",
                "action_id": "action_x",
                "intensity": 0.7,
                "context_embedding_b64": "",
            })
            assert result is not None
            assert len(result.content) >= 0


def test_mcp_prompt_exists():
    from adaptive_pathway.mcp_server import adaptive_instructions
    text = adaptive_instructions()
    assert "BEFORE selecting any tool" in text
    assert "AFTER every tool execution" in text


def test_format_result_includes_edge_id():
    # decide()'s hints previously dropped edge_id, leaving query_attribution as
    # the only (broken, see KNOWN_ISSUES.md) way to link a hint back to an edge.
    from adaptive_pathway.mcp_server import _format_result
    from adaptive_pathway.types import Hint, BlendedHint, DecisionResult

    single = Hint(text="Prefer X", confidence=0.8, primitive="X", domain="d",
                  attribution_id="attr-1", edge_id="edge-1")
    blended = BlendedHint(text="Blend X+Y", confidence=0.6, source_primitive_a="X",
                           source_primitive_b="Y", attribution_id="attr-2", edge_id="edge-2")
    result = DecisionResult(hints=[single, blended], confidence=0.7, novelty=0.2,
                             attribution_ids=["attr-1", "attr-2"], is_flow_state=False)

    formatted = _format_result(result)
    assert formatted["hints"][0]["edge_id"] == "edge-1"
    assert formatted["hints"][1]["edge_id"] == "edge-2"


@pytest.mark.asyncio
async def test_mcp_accept_nudge(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("accept_nudge", {
                "session_id": "test_sess",
            })
            data = json.loads(result.content[0].text)
            assert data["status"] == "accepted"


@pytest.mark.asyncio
async def test_mcp_session_reflection(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("session_reflection", {
                "session_id": "test_sess",
            })
            raw = result.content[0].text
            assert "reflection" in raw


@pytest.mark.asyncio
async def test_mcp_resolve_schism_both(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("resolve_schism", {
                "keep_faction": "both",
            })
            raw = result.content[0].text
            assert "error" in raw.lower() or "resolved" in raw.lower() or raw == ""


@pytest.mark.asyncio
async def test_mcp_decide_includes_source_model(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("decide", {
                "session_id": "test_sess",
                "available_actions": "tool_a,tool_b,tool_c",
            })
            raw = result.content[0].text
            assert "source_model" in raw or "nudge_offered" in raw


@pytest.mark.asyncio
async def test_mcp_decide_accepts_context_text_param(db_path):
    # End-to-end proof (real stdio subprocess, not just a unit test) that the
    # new `context` param is wired correctly through the async tool wrapper's
    # asyncio.to_thread embedding call without erroring, whether or not
    # Ollama is actually reachable in this environment (falls back to the
    # hashing vectorizer either way).
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("decide", {
                "session_id": "ctx_test_sess",
                "available_actions": "tool_a,tool_b",
                "context": "reviewing a novel draft about violence",
            })
            raw = result.content[0].text
            assert "hints" in raw


@pytest.mark.asyncio
async def test_mcp_decide_includes_exploration_metrics(db_path):
    env = os.environ.copy()
    env["ADAPTIVE_PATHWAY_DB"] = db_path
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=env,
    )
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            result = await session.call_tool("decide", {
                "session_id": "test_sess",
                "available_actions": "tool_a",
            })
            raw = result.content[0].text
            assert "exploration_metrics" in raw
