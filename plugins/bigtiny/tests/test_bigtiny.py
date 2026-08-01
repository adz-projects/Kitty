"""Comprehensive test suite: unit, integration, and robustness.
Run: pytest tests/test_bigtiny.py -v --tb=short
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any, AsyncIterator
from uuid import uuid4

import pytest
import pytest_asyncio
import yaml

from bigtiny.config import (
    BigTinyConfig,
    FallbackConfig,
    TokenManagementConfig,
    HITLConfig,
    load_config,
)
from bigtiny.storage import Database
from bigtiny.logging_config import setup_logging
from bigtiny.models.session import Message, MessageRole, Session
from bigtiny.models.provider import (
    ProviderConfig,
    ModelInfo,
    HealthStatus,
    ProviderType,
)
from bigtiny.models.mcp_server import (
    MCPServerConfig,
    ToolDefinition,
    ToolResult,
    TransportType,
)
from bigtiny.models.recipe import Recipe
from bigtiny.models.schedule import JobConfig
from bigtiny.providers.base import Provider, Delta, ToolCall
from bigtiny.providers.router import ProviderRouter, NoHealthyProvider
from bigtiny.mcp.tools import validate_tool_args, truncate_output, MAX_TOOL_OUTPUT_BYTES
from bigtiny.mcp.manager import MCPManager, MCPServerClient, MCPServerError
from bigtiny.hitl.manager import HITLManager, PendingAction, HITLDecision
from bigtiny.agent.context_manager import ContextManager, SessionStats
from bigtiny.agent import sandbox
from bigtiny.agent.loop import (
    Agent,
    _dicts_to_messages,
    _derive_title,
    _tools_to_openai_format,
)
from bigtiny.server.events import SSEEvent, serialize_sse
from bigtiny.server.middleware import add_middleware
from bigtiny.recipes.engine import RecipeEngine
from bigtiny.scheduler.scheduler import Scheduler
from bigtiny.subagent.manager import SubagentManager, Subagent
from bigtiny.discovery.discovery import LocalModelDiscovery


# =============================================================================
#  FIXTURES
# =============================================================================

@pytest.fixture
def db():
    db = Database(":memory:")

    async def setup():
        await db.connect()
        return db

    db_instance = asyncio.run(setup())
    yield db_instance
    asyncio.run(db_instance.close())


@pytest.fixture
def config():
    return BigTinyConfig()


@pytest.fixture
def token_config():
    return TokenManagementConfig()


@pytest.fixture
def hitl_config():
    return HITLConfig()


@pytest_asyncio.fixture
async def session(db):
    sid = uuid4().hex
    await db.execute(
        "INSERT INTO sessions (id, name) VALUES (:id, :name)",
        {"id": sid, "name": "test"},
    )
    return sid


@pytest_asyncio.fixture
async def router(db):
    r = ProviderRouter(db)
    return r


@pytest_asyncio.fixture
async def mcp(db):
    return MCPManager(db)


@pytest_asyncio.fixture
async def hitl(db, hitl_config):
    return HITLManager(db, hitl_config)


@pytest_asyncio.fixture
async def context(db, token_config):
    return ContextManager(db, token_config)


@pytest_asyncio.fixture
async def agent(router, mcp, hitl, context, db):
    return Agent(router, mcp, hitl, context, db)


@pytest_asyncio.fixture
async def stats(db):
    return SessionStats(db)


# =============================================================================
#  MOCK PROVIDER
# =============================================================================

class MockProvider(Provider):
    def __init__(self, provider_id: str = "mock", name: str = "mock"):
        super().__init__(
            provider_id,
            ProviderConfig(
                id=provider_id,
                name=name,
                provider_type=ProviderType.openai_compat,
                base_url="http://localhost:8000",
            ),
        )
        self._deltas: list[list[Delta]] = []
        self._call_index = 0
        self._models = [ModelInfo(id="mock-model-1")]
        self._health = HealthStatus(status="healthy", latency_ms=1.0)
        self._fail_on_call = False

    def set_deltas(self, deltas: list[list[Delta]]):
        self._deltas = deltas

    def set_health(self, health: HealthStatus):
        self._health = health

    def set_models(self, models: list[ModelInfo]):
        self._models = models

    def set_fail(self, fail: bool):
        self._fail_on_call = fail

    async def chat_completion(self, messages, tools=None, **kwargs) -> AsyncIterator[Delta]:
        if self._fail_on_call:
            raise Exception("Mock provider failure")
        if self._call_index < len(self._deltas):
            for d in self._deltas[self._call_index]:
                yield d
            self._call_index += 1
        else:
            yield Delta(content="Default mock response", finish_reason="stop")

    async def discover_models(self) -> list[ModelInfo]:
        return self._models

    async def count_tokens(self, messages: list[dict]) -> int:
        return sum(len(str(m.get("content", ""))) // 4 for m in messages)

    async def check_health(self) -> HealthStatus:
        return self._health


SAMPLE_TEXT_DELTAS = [[Delta(content="Hello world"), Delta(finish_reason="stop")]]
SAMPLE_TOOL_DELTAS = [[
    Delta(tool_calls=[ToolCall(id="call_1", function={"name": "read_file", "arguments": '{"path":"/tmp/a"}'})]),
    Delta(finish_reason="tool_calls"),
]]


# =============================================================================
#  TEST: STORAGE — Step 0
# =============================================================================

class TestStorage:
    @pytest.mark.asyncio
    async def test_connect_creates_tables(self, db):
        tables = ["schema_version", "sessions", "messages", "hitl_rules",
                   "providers", "mcp_servers", "recipes", "schedule_jobs", "execution_history"]
        for t in tables:
            row = await db.fetch_one(
                "SELECT name FROM sqlite_master WHERE type='table' AND name=:name",
                {"name": t},
            )
            assert row is not None, f"Table {t} not created"

    @pytest.mark.asyncio
    async def test_insert_and_fetch_one(self, db):
        sid = uuid4().hex
        await db.execute(
            "INSERT INTO sessions (id, name) VALUES (:id, :name)",
            {"id": sid, "name": "test"},
        )
        row = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sid})
        assert row is not None
        assert row["name"] == "test"

    @pytest.mark.asyncio
    async def test_fetch_one_none(self, db):
        row = await db.fetch_one("SELECT * FROM sessions WHERE id='nonexistent'")
        assert row is None

    @pytest.mark.asyncio
    async def test_insert_and_fetch_all(self, db):
        for i in range(3):
            sid = uuid4().hex
            await db.execute(
                "INSERT INTO sessions (id, name) VALUES (:id, :name)",
                {"id": sid, "name": f"test_{i}"},
            )
        rows = await db.fetch_all("SELECT * FROM sessions ORDER BY name")
        assert len(rows) == 3

    @pytest.mark.asyncio
    async def test_foreign_key_enforced(self, db):
        with pytest.raises(Exception):
            await db.execute(
                "INSERT INTO messages (id, session_id, role, content) VALUES (:id, :sid, 'user', 'hi')",
                {"id": uuid4().hex, "sid": "bad_session"},
            )

    @pytest.mark.asyncio
    async def test_migration_idempotent(self, db):
        await db.connect()
        tables = await db.fetch_all("SELECT name FROM sqlite_master WHERE type='table'")
        count1 = len(tables)
        await db.connect()
        tables2 = await db.fetch_all("SELECT name FROM sqlite_master WHERE type='table'")
        assert len(tables2) == count1

    @pytest.mark.asyncio
    async def test_db_not_connected_asserts(self):
        db = Database(":memory:")
        with pytest.raises(AssertionError):
            await db.execute("SELECT 1")

    @pytest.mark.asyncio
    async def test_db_path_creates_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            db_path = str(Path(tmp) / "sub" / "test.db")
            db = Database(db_path)
            await db.connect()
            assert Path(db_path).exists()
            await db.close()

    @pytest.mark.asyncio
    async def test_close_then_operate(self, db):
        await db.close()
        with pytest.raises(AssertionError):
            await db.execute("SELECT 1")


# =============================================================================
#  TEST: CONFIG — Step 0
# =============================================================================

class TestConfig:
    def test_default_config(self, config):
        assert config.server.port == 8080
        assert config.server.host == "127.0.0.1"
        assert config.token_management.max_context_tokens == 64000
        assert config.hitl.default_policy == "always_ask"
        assert config.fallback.mode == "priority"
        assert config.logging.level == "info"

    def test_yaml_override(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump({"server": {"port": 9090}}, f)
            fname = f.name
        try:
            cfg = load_config(fname)
            assert cfg.server.port == 9090
            assert cfg.server.host == "127.0.0.1"
        finally:
            os.unlink(fname)

    def test_partial_override(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            yaml.dump({"fallback": {"mode": "round-robin"}}, f)
            fname = f.name
        try:
            cfg = load_config(fname)
            assert cfg.fallback.mode == "round-robin"
            assert cfg.fallback.max_retries == 2
        finally:
            os.unlink(fname)

    def test_nested_sub_configs(self, config):
        assert isinstance(config.fallback, FallbackConfig)
        assert isinstance(config.token_management, TokenManagementConfig)
        assert isinstance(config.hitl, HITLConfig)

    def test_config_default_path(self):
        cfg = load_config()
        assert cfg.server.port == 8080

    def test_hitl_reject_patterns_defaults(self, config):
        assert "rm -rf /" in config.hitl.auto_reject_patterns


# =============================================================================
#  TEST: PROVIDERS — Step 1
# =============================================================================

class TestProviderBase:
    def test_tool_call_model(self):
        tc = ToolCall(id="call_1", function={"name": "read_file", "arguments": '{"path":"/tmp/a"}'})
        assert tc.id == "call_1"
        assert tc.function["name"] == "read_file"

    def test_delta_model(self):
        d = Delta(content="Hello", finish_reason="stop")
        assert d.content == "Hello"
        assert d.finish_reason == "stop"

    def test_delta_with_tool_calls(self):
        d = Delta(tool_calls=[ToolCall(id="c1", function={"name": "t"})])
        assert d.tool_calls is not None
        assert d.tool_calls[0].id == "c1"


class TestOpenAIProvider:
    @pytest.mark.asyncio
    async def test_serialize_message(self):
        from bigtiny.providers.openai_compat import OpenAICompatibleProvider
        prov = OpenAICompatibleProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.openai_compat, base_url="http://localhost"))
        msg = Message(session_id="s1", role=MessageRole.user, content="Hello")
        result = prov._serialize_message(msg)
        assert result["role"] == "user"
        assert result["content"] == "Hello"

    @pytest.mark.asyncio
    async def test_parse_chunk_valid(self):
        from bigtiny.providers.openai_compat import OpenAICompatibleProvider
        prov = OpenAICompatibleProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.openai_compat, base_url="http://localhost"))
        data = '{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}'
        delta = prov._parse_chunk(data, {})
        assert delta is not None
        assert delta.content == "Hello"

    @pytest.mark.asyncio
    async def test_parse_chunk_invalid_json(self):
        from bigtiny.providers.openai_compat import OpenAICompatibleProvider
        prov = OpenAICompatibleProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.openai_compat, base_url="http://localhost"))
        assert prov._parse_chunk("not json", {}) is None

    @pytest.mark.asyncio
    async def test_parse_chunk_tool_calls(self):
        from bigtiny.providers.openai_compat import OpenAICompatibleProvider
        prov = OpenAICompatibleProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.openai_compat, base_url="http://localhost"))
        # Streamed tool calls arrive as fragments; they accumulate in the
        # buffer and are only assembled into ToolCalls at end of stream.
        buf: dict[int, dict[str, str]] = {}
        first = '{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\\"pa"}}]},"finish_reason":null}]}'
        second = '{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\\": \\"x\\"}"}}]},"finish_reason":null}]}'
        assert prov._parse_chunk(first, buf) is None
        assert prov._parse_chunk(second, buf) is None
        calls = prov._assemble_tool_calls(buf)
        assert calls is not None
        assert calls[0].id == "c1"
        assert calls[0].function["name"] == "read_file"
        assert json.loads(calls[0].function["arguments"]) == {"path": "x"}

    def test_encoding_cache(self):
        from bigtiny.providers.openai_compat import ENCODING_CACHE
        ENCODING_CACHE.clear()
        from bigtiny.providers.openai_compat import _get_encoding
        enc = _get_encoding("gpt-4")
        assert enc is not None
        assert "gpt-4" in ENCODING_CACHE
        enc2 = _get_encoding("gpt-4")
        assert enc is enc2


class TestThinkTagSplitter:
    """Some OpenAI-compatible backends (Ollama's /v1/chat/completions for
    reasoning models) emit `<think>...</think>` inline in `content` instead of
    a distinct reasoning field. The splitter routes it to the reasoning
    channel so the client never has to see the raw tags."""

    def test_no_tags_passes_through_as_content(self):
        from bigtiny.providers.openai_compat import ThinkTagSplitter
        s = ThinkTagSplitter()
        content, reasoning = s.feed("just a normal answer")
        assert content == "just a normal answer"
        assert reasoning == ""

    def test_single_chunk_full_block(self):
        from bigtiny.providers.openai_compat import ThinkTagSplitter
        s = ThinkTagSplitter()
        content, reasoning = s.feed("<think>planning</think>the answer")
        assert content == "the answer"
        assert reasoning == "planning"

    def test_tag_split_across_chunks(self):
        from bigtiny.providers.openai_compat import ThinkTagSplitter
        s = ThinkTagSplitter()
        c1, r1 = s.feed("<th")
        c2, r2 = s.feed("ink>reasoning")
        c3, r3 = s.feed(" continues</th")
        c4, r4 = s.feed("ink>answer")
        assert (c1, r1) == ("", "")
        assert (c2, r2) == ("", "reasoning")
        assert (c3, r3) == ("", " continues")
        assert (c4, r4) == ("answer", "")

    def test_content_before_and_after_think_block(self):
        from bigtiny.providers.openai_compat import ThinkTagSplitter
        s = ThinkTagSplitter()
        content, reasoning = s.feed("intro<think>hidden</think>outro")
        assert content == "introoutro"
        assert reasoning == "hidden"

    def test_unclosed_think_block_streams_as_reasoning_progressively(self):
        # A truncated/cancelled turn can end mid-thought — the dangling text
        # must still surface (as reasoning, since it never closed) rather
        # than vanish silently. It streams out via feed() itself (not held
        # for flush()) since there's no ambiguity once past the open tag.
        from bigtiny.providers.openai_compat import ThinkTagSplitter
        s = ThinkTagSplitter()
        _, reasoning = s.feed("<think>never finished")
        assert reasoning == "never finished"
        assert s.flush() == ("", "")

    def test_flush_emits_a_close_tag_fragment_cut_off_mid_marker(self):
        # The stream can end with a partial tag still held back speculatively
        # (see `_partial_tag_holdback`) — e.g. cut off right after "</th".
        # flush() must surface that fragment (as reasoning, since it was
        # never confirmed to be a real closing tag) rather than drop it.
        from bigtiny.providers.openai_compat import ThinkTagSplitter
        s = ThinkTagSplitter()
        s.feed("<think>reasoning tail</th")
        content, reasoning = s.flush()
        assert content == ""
        assert reasoning == "</th"

    def test_flush_emits_nothing_when_buffer_already_empty(self):
        from bigtiny.providers.openai_compat import ThinkTagSplitter
        s = ThinkTagSplitter()
        s.feed("<think>done</think>answer")
        assert s.flush() == ("", "")

    @pytest.mark.asyncio
    async def test_chat_completion_splits_inline_think_tags(self, monkeypatch):
        from bigtiny.providers.openai_compat import OpenAICompatibleProvider

        prov = OpenAICompatibleProvider(
            "test",
            ProviderConfig(id="test", name="t", provider_type=ProviderType.openai_compat, base_url="http://localhost"),
        )

        class FakeResponse:
            status_code = 200

            async def aiter_lines(self):
                for chunk in [
                    '{"choices":[{"delta":{"content":"<think>"},"finish_reason":null}]}',
                    '{"choices":[{"delta":{"content":"reasoning here"},"finish_reason":null}]}',
                    '{"choices":[{"delta":{"content":"</think>final answer"},"finish_reason":null}]}',
                    '{"choices":[{"delta":{},"finish_reason":"stop"}]}',
                    "[DONE]",
                ]:
                    yield f"data: {chunk}" if chunk != "[DONE]" else "data: [DONE]"

            async def __aenter__(self):
                return self

            async def __aexit__(self, *a):
                return False

        class FakeStreamCtx:
            def __call__(self, *a, **kw):
                return FakeResponse()

        monkeypatch.setattr(prov._client, "stream", FakeStreamCtx())

        deltas = [d async for d in prov.chat_completion([
            Message(session_id="s1", role=MessageRole.user, content="hi")
        ])]
        full_content = "".join(d.content or "" for d in deltas)
        full_reasoning = "".join(d.reasoning or "" for d in deltas)
        assert full_content == "final answer"
        assert full_reasoning == "reasoning here"


class TestAnthropicProvider:
    @pytest.mark.asyncio
    async def test_serialize_message(self):
        from bigtiny.providers.anthropic import AnthropicProvider
        prov = AnthropicProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.anthropic, base_url="https://api.anthropic.com"))
        msg = Message(session_id="s1", role=MessageRole.user, content="Hello")
        result = prov._serialize_message(msg)
        assert result["role"] == "user"
        assert result["content"] == "Hello"

    @pytest.mark.asyncio
    async def test_parse_text_delta(self):
        from bigtiny.providers.anthropic import AnthropicProvider
        prov = AnthropicProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.anthropic, base_url="https://api.anthropic.com"))
        data = {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Hello"}}
        delta = prov._parse_event(data)
        assert delta is not None
        assert delta.content == "Hello"

    @pytest.mark.asyncio
    async def test_parse_tool_use_block(self):
        from bigtiny.providers.anthropic import AnthropicProvider
        prov = AnthropicProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.anthropic, base_url="https://api.anthropic.com"))
        # tool_use blocks are accumulated by the streaming loop (input arrives
        # via input_json_delta fragments), so _parse_event ignores them.
        data = {"type": "content_block_start", "content_block": {"type": "tool_use", "id": "tu1", "name": "read_file", "input": {}}}
        assert prov._parse_event(data) is None

    @pytest.mark.asyncio
    async def test_parse_message_stop(self):
        from bigtiny.providers.anthropic import AnthropicProvider
        prov = AnthropicProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.anthropic, base_url="https://api.anthropic.com"))
        # message_stop is handled inline by the streaming loop (it carries
        # usage); _parse_event handles message_delta stop reasons.
        data = {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}
        delta = prov._parse_event(data)
        assert delta is not None
        assert delta.finish_reason == "end_turn"

    @pytest.mark.asyncio
    async def test_convert_tools(self):
        from bigtiny.providers.anthropic import AnthropicProvider
        prov = AnthropicProvider("test", ProviderConfig(id="test", name="t", provider_type=ProviderType.anthropic, base_url="https://api.anthropic.com"))
        tools = [{"function": {"name": "read", "description": "Read", "parameters": {"type": "object"}}}]
        result = prov._convert_tools(tools)
        assert result[0]["name"] == "read"


class TestAnthropicToolResults:
    """`_serialize_message`/`_build_messages` used to flatten every tool
    reply into a synthetic plain-text `user` message, discarding
    `tool_call_id` correlation entirely. These cover the fix: proper
    `tool_use`/`tool_result` content blocks, correctly paired by id, with
    consecutive tool replies coalesced into one `user` message (required —
    Anthropic rejects back-to-back `user` messages)."""

    def _prov(self):
        from bigtiny.providers.anthropic import AnthropicProvider
        return AnthropicProvider(
            "test",
            ProviderConfig(id="test", name="t", provider_type=ProviderType.anthropic, base_url="https://api.anthropic.com"),
        )

    def test_assistant_tool_calls_become_tool_use_blocks(self):
        prov = self._prov()
        msg = Message(
            session_id="s1", role=MessageRole.assistant, content="",
            tool_calls=[{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": '{"path":"/a"}'}}],
        )
        result = prov._serialize_message(msg)
        assert result["role"] == "assistant"
        assert result["content"] == [
            {"type": "tool_use", "id": "call_1", "name": "read_file", "input": {"path": "/a"}}
        ]

    def test_assistant_tool_calls_keep_leading_text_block(self):
        prov = self._prov()
        msg = Message(
            session_id="s1", role=MessageRole.assistant, content="Let me check that.",
            tool_calls=[{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}],
        )
        result = prov._serialize_message(msg)
        assert result["content"][0] == {"type": "text", "text": "Let me check that."}
        assert result["content"][1]["type"] == "tool_use"

    def test_plain_text_message_unaffected(self):
        # Existing behavior (no tool_calls) must be untouched by the new branch.
        prov = self._prov()
        msg = Message(session_id="s1", role=MessageRole.user, content="Hello")
        result = prov._serialize_message(msg)
        assert result == {"role": "user", "content": "Hello"}

    def test_consecutive_tool_results_coalesce_into_one_user_message(self):
        prov = self._prov()
        messages = [
            Message(session_id="s1", role=MessageRole.user, content="do it"),
            Message(session_id="s1", role=MessageRole.assistant, content="", tool_calls=[
                {"id": "call_1", "type": "function", "function": {"name": "a", "arguments": "{}"}},
                {"id": "call_2", "type": "function", "function": {"name": "b", "arguments": "{}"}},
            ]),
            Message(session_id="s1", role=MessageRole.tool, content="result-a", tool_call_id="call_1"),
            Message(session_id="s1", role=MessageRole.tool, content="result-b", tool_call_id="call_2"),
            Message(session_id="s1", role=MessageRole.assistant, content="done"),
        ]
        built = prov._build_messages(messages)
        assert [m["role"] for m in built] == ["user", "assistant", "user", "assistant"]
        assert built[2]["content"] == [
            {"type": "tool_result", "tool_use_id": "call_1", "content": "result-a"},
            {"type": "tool_result", "tool_use_id": "call_2", "content": "result-b"},
        ]

    def test_system_messages_excluded_from_build_messages(self):
        prov = self._prov()
        messages = [
            Message(session_id="s1", role=MessageRole.system, content="sys"),
            Message(session_id="s1", role=MessageRole.user, content="hi"),
        ]
        built = prov._build_messages(messages)
        assert len(built) == 1
        assert built[0]["role"] == "user"


# =============================================================================
#  TEST: ROUTER — Step 1
# =============================================================================

class TestRouter:
    @pytest.mark.asyncio
    async def test_empty_router_raises_no_healthy(self, router):
        with pytest.raises(NoHealthyProvider):
            await router.get_provider()

    @pytest.mark.asyncio
    async def test_load_providers_from_db(self, db, router):
        await db.execute(
            "INSERT INTO providers (id, name, provider_type, base_url) "
            "VALUES (:id, :name, 'openai_compat', :url)",
            {"id": "p1", "name": "test", "url": "http://localhost:8000"},
        )
        await router.load_providers()
        assert "p1" in router._providers

    @pytest.mark.asyncio
    async def test_get_provider_ids(self, db, router):
        await db.execute(
            "INSERT INTO providers (id, name, provider_type, base_url) "
            "VALUES (:id, :name, 'openai_compat', :url)",
            {"id": "p1", "name": "test", "url": "http://localhost:8000"},
        )
        await router.load_providers()
        ids = router.get_provider_ids()
        assert "p1" in ids

    @pytest.mark.asyncio
    async def test_check_all_health_empty(self, router):
        result = await router.check_all_health()
        assert result == {}

    def test_instantiate_openai(self, router):
        row = {"id": "p1", "name": "t", "provider_type": "openai_compat",
               "base_url": "http://localhost", "fallback_priority": 1, "status": "disconnected"}
        p = router._instantiate(row, None)
        from bigtiny.providers.openai_compat import OpenAICompatibleProvider
        assert isinstance(p, OpenAICompatibleProvider)

    def test_instantiate_anthropic(self, router):
        row = {"id": "p2", "name": "t", "provider_type": "anthropic",
               "base_url": "https://api.anthropic.com", "fallback_priority": 1, "status": "disconnected"}
        p = router._instantiate(row, None)
        from bigtiny.providers.anthropic import AnthropicProvider
        assert isinstance(p, AnthropicProvider)


# =============================================================================
#  TEST: MCP — Step 2
# =============================================================================

class TestMCPTools:
    def test_validate_tool_args_passes(self):
        td = ToolDefinition(name="r", description="Read", input_schema={
            "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"],
        }, server_id="fs")
        result = validate_tool_args(td, {"path": "/tmp/a"})
        assert result == {"path": "/tmp/a"}

    def test_validate_tool_args_missing_required(self):
        td = ToolDefinition(name="r", description="Read", input_schema={
            "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"],
        }, server_id="fs")
        with pytest.raises(ValueError, match="required"):
            validate_tool_args(td, {})

    def test_validate_tool_args_extra_allowed(self):
        td = ToolDefinition(name="r", description="Read", input_schema={
            "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"],
        }, server_id="fs")
        result = validate_tool_args(td, {"path": "/tmp/a", "extra": 1})
        assert result == {"path": "/tmp/a", "extra": 1}

    def test_validate_tool_args_empty_schema(self):
        td = ToolDefinition(name="r", description="Read", input_schema={}, server_id="fs")
        result = validate_tool_args(td, {"anything": "goes"})
        assert result == {"anything": "goes"}

    def test_truncate_output_small(self):
        content = "small" * 100
        result, truncated = truncate_output(content)
        assert result == content
        assert not truncated

    def test_truncate_output_large(self):
        content = "x" * (MAX_TOOL_OUTPUT_BYTES + 1)
        result, truncated = truncate_output(content)
        assert truncated
        assert len(result.encode("utf-8")) <= MAX_TOOL_OUTPUT_BYTES + 200
        assert "truncated" in result

    def test_truncate_exact_boundary(self):
        content = "x" * MAX_TOOL_OUTPUT_BYTES
        result, truncated = truncate_output(content)
        assert not truncated
        assert result == content


class TestMCPManager:
    @pytest.mark.asyncio
    async def test_connect_not_found(self, mcp):
        with pytest.raises(MCPServerError):
            await mcp.connect_server("nonexistent")

    @pytest.mark.asyncio
    async def test_list_tools_empty(self, mcp):
        tools = await mcp.list_tools()
        assert tools == []

    @pytest.mark.asyncio
    async def test_execute_unknown_tool(self, mcp):
        result = await mcp.execute_tool("nonexistent", {})
        assert result.is_error
        assert "Unknown" in result.content

    @pytest.mark.asyncio
    async def test_disconnect_all_idempotent(self, mcp):
        await mcp.disconnect_all()

    @pytest.mark.asyncio
    async def test_disconnect_server_not_connected(self, mcp):
        await mcp.disconnect_server("nonexistent")

    @pytest.mark.asyncio
    async def test_connect_all_skips_disabled(self, db, mcp):
        await db.execute(
            "INSERT INTO mcp_servers (id, name, transport, command, args, env, enabled) "
            "VALUES (:id, :name, 'stdio', 'nonexistent-binary', '[]', '{}', 0)",
            {"id": "disabled-server", "name": "disabled"},
        )
        # Should not attempt to connect the disabled row (which would fail
        # since the binary doesn't exist) and therefore not raise/log an
        # attempted connection at all — assert via absence of a client.
        await mcp.connect_all()
        assert "disabled-server" not in mcp._servers

    @pytest.mark.asyncio
    async def test_client_execute_unknown_tool_returns_error_result_not_raise(self):
        # Regression: `MCPServerClient.execute_tool` used to `raise
        # MCPServerError` here (before entering its own try block), which
        # `MCPManager.execute_tool` didn't catch either — an unexpected
        # exception this deep would propagate uncaught through
        # `Agent._run_one_tool_call` into the turn's `asyncio.gather`
        # (not given `return_exceptions=True`), killing every concurrent
        # tool call in the turn instead of just failing this one call.
        client = MCPServerClient(MCPServerConfig(id="s1", name="s1", transport=TransportType.stdio))
        client._tools = []  # tool_name won't be found
        result = await client.execute_tool("nonexistent", {})
        assert result.is_error
        assert "Unknown tool" in result.content

    @pytest.mark.asyncio
    async def test_client_execute_tool_unexpected_exception_returns_error_result(self):
        # Simulates a failure mode `_send_request` isn't explicitly written
        # to raise (e.g. a dropped connection, or a bug in `_extract_content`)
        # — anything beyond the two expected `TimeoutError`/`MCPServerError`
        # cases must still come back as an error ToolResult, not propagate.
        tool_def = ToolDefinition(name="t", description="", input_schema={}, server_id="s1")
        client = MCPServerClient(MCPServerConfig(id="s1", name="s1", transport=TransportType.stdio))
        client._tools = [tool_def]

        async def _boom(*args, **kwargs):
            raise RuntimeError("connection reset")

        client._send_request = _boom
        result = await client.execute_tool("t", {})
        assert result.is_error
        assert "failed unexpectedly" in result.content

    @pytest.mark.asyncio
    async def test_manager_execute_tool_survives_client_raising_unexpectedly(self, mcp):
        # Defense-in-depth check on `MCPManager.execute_tool`'s own wrapper:
        # even if some future/buggy client implementation raises instead of
        # returning an error ToolResult, the manager-level dispatch (the
        # actual call site `Agent._run_one_tool_call` uses) must not let
        # that exception escape.
        tool_def = ToolDefinition(name="boom_tool", description="", input_schema={}, server_id="s1")
        mcp._tool_registry["boom_tool"] = tool_def

        class _RaisingClient:
            async def execute_tool(self, *args, **kwargs):
                raise RuntimeError("simulated client bug")

        mcp._servers["s1"] = _RaisingClient()
        result = await mcp.execute_tool("boom_tool", {})
        assert result.is_error
        assert "failed unexpectedly" in result.content

    @pytest.mark.asyncio
    async def test_connect_all_includes_enabled_by_default(self, db):
        # enabled has DEFAULT 1, so a row inserted without specifying it
        # should still be picked up by connect_all's WHERE clause.
        await db.execute(
            "INSERT INTO mcp_servers (id, name, transport, command, args, env) "
            "VALUES (:id, :name, 'stdio', 'nonexistent-binary', '[]', '{}')",
            {"id": "default-enabled-server", "name": "default-enabled"},
        )
        row = await db.fetch_one(
            "SELECT enabled FROM mcp_servers WHERE id = :id",
            {"id": "default-enabled-server"},
        )
        assert row["enabled"] == 1


class _FakeStdinWriter:
    """Stand-in for `asyncio.StreamWriter` — records writes, never actually IO."""
    def __init__(self):
        self.written: list[bytes] = []

    def write(self, data: bytes) -> None:
        self.written.append(data)

    async def drain(self) -> None:
        pass


class _FakeStdoutReader:
    """Stand-in for `asyncio.StreamReader` — hands back pre-loaded lines in
    order, then EOF (empty bytes) once exhausted. The `asyncio.sleep(0)` is
    load-bearing, not decorative: real stdio I/O always genuinely suspends,
    which is what lets a concurrent `_send_stdio_request` call get a turn to
    register its future *before* the reader loop delivers a response for it.
    Without it, this fake (whose body never truly awaits anything) would let
    `_stdio_reader_loop` run every queued line to completion in one scheduler
    turn, before either request coroutine ever ran — silently discarding
    both "responses" and leaving the real requests' futures orphaned forever."""
    def __init__(self, lines: list[bytes]):
        self._lines = list(lines)

    async def readline(self) -> bytes:
        await asyncio.sleep(0)
        if self._lines:
            return self._lines.pop(0)
        return b""


class TestStdioTransportConcurrency:
    """Regression coverage for the stdio race fix: before it, each
    `_send_stdio_request` call ran its own read loop directly against the
    shared `self._reader`, so two concurrent calls to the same stdio server
    could each consume the other's response line — silent corruption. The
    fix routes every response through one background reader task
    (`_stdio_reader_loop`) that demultiplexes by JSON-RPC id into a
    futures map, so arrival order no longer matters."""

    @pytest.mark.asyncio
    async def test_concurrent_requests_survive_out_of_order_responses(self):
        config = MCPServerConfig(id="s1", name="s1", transport=TransportType.stdio, command="fake")
        client = MCPServerClient(config)
        # The response for id=2 arrives on the wire *before* id=1's — exactly
        # the interleaving that used to let one caller's read loop steal the
        # other's line.
        lines = [
            json.dumps({"jsonrpc": "2.0", "id": 2, "result": {"value": "two"}}).encode() + b"\n",
            json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"value": "one"}}).encode() + b"\n",
        ]
        client._reader = _FakeStdoutReader(lines)
        client._writer = _FakeStdinWriter()
        client._stdio_reader_task = asyncio.create_task(client._stdio_reader_loop())
        try:
            result1, result2 = await asyncio.gather(
                client._send_stdio_request({"jsonrpc": "2.0", "id": 1, "method": "x", "params": {}}),
                client._send_stdio_request({"jsonrpc": "2.0", "id": 2, "method": "y", "params": {}}),
            )
            assert result1 == {"value": "one"}
            assert result2 == {"value": "two"}
            assert client._pending_stdio == {}  # both futures cleaned up
        finally:
            client._stdio_reader_task.cancel()

    @pytest.mark.asyncio
    async def test_error_response_rejects_only_the_matching_request(self):
        config = MCPServerConfig(id="s1", name="s1", transport=TransportType.stdio, command="fake")
        client = MCPServerClient(config)
        lines = [
            json.dumps({"jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": "boom"}}).encode() + b"\n",
            json.dumps({"jsonrpc": "2.0", "id": 2, "result": {"value": "ok"}}).encode() + b"\n",
        ]
        client._reader = _FakeStdoutReader(lines)
        client._writer = _FakeStdinWriter()
        client._stdio_reader_task = asyncio.create_task(client._stdio_reader_loop())
        try:
            results = await asyncio.gather(
                client._send_stdio_request({"jsonrpc": "2.0", "id": 1, "method": "x", "params": {}}),
                client._send_stdio_request({"jsonrpc": "2.0", "id": 2, "method": "y", "params": {}}),
                return_exceptions=True,
            )
            assert isinstance(results[0], MCPServerError)
            assert results[1] == {"value": "ok"}
        finally:
            client._stdio_reader_task.cancel()

    @pytest.mark.asyncio
    async def test_eof_rejects_any_still_pending_requests(self):
        config = MCPServerConfig(id="s1", name="s1", transport=TransportType.stdio, command="fake")
        client = MCPServerClient(config)
        client._reader = _FakeStdoutReader([])  # immediate EOF
        client._writer = _FakeStdinWriter()
        client._stdio_reader_task = asyncio.create_task(client._stdio_reader_loop())
        with pytest.raises(MCPServerError):
            await client._send_stdio_request({"jsonrpc": "2.0", "id": 1, "method": "x", "params": {}})


class _FakeStreamableResponse:
    """Stand-in for an `httpx.Response` from a Streamable HTTP MCP server."""

    def __init__(self, *, content_type="application/json", json_body=None, text_body="", headers=None):
        self.headers = {"content-type": content_type, **(headers or {})}
        self._json_body = json_body
        self._text_body = text_body
        self.status_code = 200

    def raise_for_status(self):
        pass

    def json(self):
        return self._json_body

    @property
    def text(self):
        return self._text_body


class TestStreamableHttpTransport:
    """Confirmed live against a real deployment (a personal RAG MCP server):
    a single POST endpoint, SSE-framed JSON responses, Bearer auth via a
    client-level header, and no session id (stateless) — but the spec allows
    a server to hand back `Mcp-Session-Id` too, so that path is covered as
    well rather than assumed absent."""

    def test_parse_sse_body_reads_first_data_line(self):
        text = 'event: message\ndata: {"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n\n'
        assert MCPServerClient._parse_sse_body(text) == {
            "jsonrpc": "2.0", "id": 1, "result": {"ok": True},
        }

    def test_parse_sse_body_returns_none_without_a_data_line(self):
        assert MCPServerClient._parse_sse_body("event: message\n\n") is None

    @pytest.mark.asyncio
    async def test_init_streamable_http_sets_auth_header_on_the_client(self):
        config = MCPServerConfig(
            id="s1", name="rag", transport=TransportType.streamable_http,
            url="http://example.test/mcp/", headers={"Authorization": "Bearer tok"},
        )
        client = MCPServerClient(config)
        await client._init_streamable_http()
        try:
            assert client._http_client.headers.get("authorization") == "Bearer tok"
        finally:
            await client._http_client.aclose()

    @pytest.mark.asyncio
    async def test_send_request_parses_sse_framed_response(self):
        config = MCPServerConfig(
            id="s1", name="rag", transport=TransportType.streamable_http,
            url="http://example.test/mcp/",
        )
        client = MCPServerClient(config)
        await client._init_streamable_http()

        async def fake_post(url, json=None, headers=None):
            return _FakeStreamableResponse(
                content_type="text/event-stream",
                text_body='event: message\ndata: {"jsonrpc":"2.0","id":1,"result":{"tools":[]}}\n\n',
            )
        client._http_client.post = fake_post

        result = await client._send_streamable_http_request(
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}
        )
        assert result == {"tools": []}

    @pytest.mark.asyncio
    async def test_send_request_parses_plain_json_response(self):
        config = MCPServerConfig(
            id="s1", name="rag", transport=TransportType.streamable_http,
            url="http://example.test/mcp/",
        )
        client = MCPServerClient(config)
        await client._init_streamable_http()

        async def fake_post(url, json=None, headers=None):
            return _FakeStreamableResponse(
                content_type="application/json",
                json_body={"jsonrpc": "2.0", "id": 1, "result": {"ok": True}},
            )
        client._http_client.post = fake_post

        result = await client._send_streamable_http_request(
            {"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}
        )
        assert result == {"ok": True}

    @pytest.mark.asyncio
    async def test_send_request_raises_on_jsonrpc_error(self):
        config = MCPServerConfig(
            id="s1", name="rag", transport=TransportType.streamable_http,
            url="http://example.test/mcp/",
        )
        client = MCPServerClient(config)
        await client._init_streamable_http()

        async def fake_post(url, json=None, headers=None):
            return _FakeStreamableResponse(
                content_type="application/json",
                json_body={"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "no such tool"}},
            )
        client._http_client.post = fake_post

        with pytest.raises(MCPServerError, match="no such tool"):
            await client._send_streamable_http_request(
                {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}}
            )

    @pytest.mark.asyncio
    async def test_session_id_captured_and_echoed_on_later_requests(self):
        # Most real deployments (including the one this was built against)
        # are stateless and never send this header — but the spec allows a
        # server to require it, so a client that DOES send one must have it
        # echoed back on every later request.
        config = MCPServerConfig(
            id="s1", name="rag", transport=TransportType.streamable_http,
            url="http://example.test/mcp/",
        )
        client = MCPServerClient(config)
        await client._init_streamable_http()

        seen_session_headers = []

        async def fake_post(url, json=None, headers=None):
            seen_session_headers.append(headers.get("Mcp-Session-Id"))
            return _FakeStreamableResponse(
                content_type="application/json",
                json_body={"jsonrpc": "2.0", "id": 1, "result": {}},
                headers={"mcp-session-id": "sess-abc123"},
            )
        client._http_client.post = fake_post

        await client._send_streamable_http_request({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        await client._send_streamable_http_request({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})

        assert seen_session_headers[0] is None
        assert seen_session_headers[1] == "sess-abc123"

    @pytest.mark.asyncio
    async def test_notification_does_not_try_to_parse_a_response_body(self):
        config = MCPServerConfig(
            id="s1", name="rag", transport=TransportType.streamable_http,
            url="http://example.test/mcp/",
        )
        client = MCPServerClient(config)
        await client._init_streamable_http()

        async def fake_post(url, json=None, headers=None):
            return _FakeStreamableResponse(content_type="application/json", text_body="")
        client._http_client.post = fake_post

        # Must not raise — a notification gets a 202/empty body, no JSON to parse.
        await client._send_streamable_http_notification(
            {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}
        )


# =============================================================================
#  TEST: HITL — Step 3
# =============================================================================

class TestHITL:
    @pytest.mark.asyncio
    async def test_auto_reject_pattern(self, hitl, session):
        result = await hitl.check_tool_call(session, "bash", {"command": "rm -rf /"})
        assert result.action == "rejected"

    @pytest.mark.asyncio
    async def test_auto_reject_partial_match(self, hitl, session):
        result = await hitl.check_tool_call(session, "bash", {"command": "chmod 777 /tmp/test"})
        assert result.action == "rejected"

    @pytest.mark.asyncio
    async def test_always_allow_whitelisted(self, hitl, session):
        result = await hitl.check_tool_call(session, "read_file", {"path": "/tmp/a"})
        assert result.action == "needs_approval"

    @pytest.mark.asyncio
    async def test_needs_approval_creates_pending(self, hitl, session):
        result = await hitl.check_tool_call(session, "custom_tool", {"arg": 1})
        assert result.action == "needs_approval"
        assert result.pending_action_id is not None
        pending = hitl.get_pending_approvals(session)
        assert len(pending) == 1
        assert pending[0].tool_name == "custom_tool"

    @pytest.mark.asyncio
    async def test_record_decision_allow(self, hitl, session):
        result = await hitl.check_tool_call(session, "t", {"a": 1})
        decision = await hitl.record_decision(result.pending_action_id, "allow")
        assert decision.action == "proceed"
        pending = hitl.get_pending_approvals(session)
        assert len(pending) == 0

    @pytest.mark.asyncio
    async def test_record_decision_reject(self, hitl, session):
        result = await hitl.check_tool_call(session, "t", {"a": 1})
        decision = await hitl.record_decision(result.pending_action_id, "reject")
        assert decision.action == "rejected"

    @pytest.mark.asyncio
    async def test_record_decision_always_allow_persists(self, hitl, session):
        result = await hitl.check_tool_call(session, "t2", {"b": 2})
        await hitl.record_decision(result.pending_action_id, "always_allow")
        next_result = await hitl.check_tool_call(session, "t2", {"b": 2})
        assert next_result.action == "proceed"

    @pytest.mark.asyncio
    async def test_cancel_pending(self, hitl, session):
        await hitl.check_tool_call(session, "t", {"a": 1})
        await hitl.cancel_pending(session)
        pending = hitl.get_pending_approvals(session)
        assert len(pending) == 0

    @pytest.mark.asyncio
    async def test_unique_action_ids(self, hitl, session):
        r1 = await hitl.check_tool_call(session, "t1", {"a": 1})
        r2 = await hitl.check_tool_call(session, "t2", {"b": 2})
        assert r1.pending_action_id != r2.pending_action_id

    @pytest.mark.asyncio
    async def test_record_decision_unknown_action(self, hitl, session):
        result = await hitl.record_decision("nonexistent", "allow")
        assert result.action == "rejected"

    @pytest.mark.asyncio
    async def test_db_rules_loaded(self, db, hitl_config, session):
        await db.execute(
            "INSERT INTO hitl_rules (tool_name, args_pattern, decision) VALUES (:n, :p, :d)",
            {"n": "db_tool", "p": None, "d": "always_allow"},
        )
        hitl2 = HITLManager(db, hitl_config)
        result = await hitl2.check_tool_call(session, "db_tool", {})
        assert result.action == "proceed"

    def test_pending_action_to_dict(self):
        pa = PendingAction("a1", "s1", "tool1", {"k": "v"})
        d = pa.to_dict()
        assert d["action_id"] == "a1"
        assert d["tool_name"] == "tool1"

    def test_hitl_decision_attributes(self):
        d = HITLDecision("proceed", reason="ok")
        assert d.action == "proceed"
        assert d.reason == "ok"

    def test_force_approval_creates_pending_action(self, hitl, session):
        decision = hitl.force_approval(session, "read_file", {"path": "/outside"})
        assert decision.action == "needs_approval"
        assert decision.pending_action_id is not None
        pending = hitl.get_pending_approvals(session)
        assert len(pending) == 1
        assert pending[0].action_id == decision.pending_action_id

    @pytest.mark.asyncio
    async def test_force_approval_bypasses_persisted_always_allow_rule(
        self, db, hitl_config, session
    ):
        # A tool with a persisted always_allow DB rule normally proceeds
        # outright — force_approval must still be able to gate it (the
        # sandbox's job: an always_allow rule means "stop asking about this
        # tool," not "let it touch arbitrary directories").
        await db.execute(
            "INSERT INTO hitl_rules (tool_name, args_pattern, decision) VALUES (:n, :p, :d)",
            {"n": "read_file", "p": None, "d": "always_allow"},
        )
        hitl2 = HITLManager(db, hitl_config)
        normal = await hitl2.check_tool_call(session, "read_file", {"path": "/outside"})
        assert normal.action == "proceed"
        forced = hitl2.force_approval(session, "read_file", {"path": "/outside"})
        assert forced.action == "needs_approval"

    @pytest.mark.asyncio
    async def test_stale_pending_action_is_evicted(self, hitl, session):
        # An approval the client never answers (disconnect, force-quit
        # without /cancel) must not leak its _pending/_session_pending
        # entries forever — the sweep in `_sweep_stale` (run opportunistically
        # from `_create_pending`) is what reclaims it.
        result = await hitl.check_tool_call(session, "abandoned_tool", {"a": 1})
        pending = hitl._pending[result.pending_action_id]
        pending.created_at = datetime.utcnow() - timedelta(hours=2)

        # Triggers another sweep by creating a second (fresh) pending action.
        await hitl.check_tool_call(session, "other_tool", {})

        assert result.pending_action_id not in hitl._pending
        assert not any(p.tool_name == "abandoned_tool" for p in hitl.get_pending_approvals(session))

    @pytest.mark.asyncio
    async def test_stale_resolved_decision_is_evicted(self, hitl, session):
        result = await hitl.check_tool_call(session, "t", {"a": 1})
        await hitl.record_decision(result.pending_action_id, "allow")
        assert result.pending_action_id in hitl._decisions

        # Back-date the resolution so the next sweep considers it stale.
        decision, _ = hitl._decisions[result.pending_action_id]
        hitl._decisions[result.pending_action_id] = (decision, datetime.utcnow() - timedelta(hours=2))

        await hitl.check_tool_call(session, "trigger_sweep", {})
        assert result.pending_action_id not in hitl._decisions

    @pytest.mark.asyncio
    async def test_recent_pending_action_survives_sweep(self, hitl, session):
        result = await hitl.check_tool_call(session, "recent_tool", {})
        await hitl.check_tool_call(session, "another_tool", {})
        assert result.pending_action_id in hitl._pending


class TestSandbox:
    """`bigtiny/agent/sandbox.py` — the authoritative, mode-dependent
    directory-containment gate (see the module's own docstring for why this
    exists as a server-side check, not just Kitty's client-side pre-filter)."""

    def test_path_within_any_matches_one_of_several_bases(self):
        assert sandbox.path_within_any(["C:/a", "C:/b"], "C:/b/file.txt")
        assert not sandbox.path_within_any(["C:/a", "C:/b"], "C:/c/file.txt")

    def test_path_within_any_resolves_relative_against_base(self):
        assert sandbox.path_within_any(["C:/chat1"], "notes.txt")
        assert sandbox.path_within_any(["C:/chat1"], "./sub/notes.txt")

    def test_path_within_any_collapses_dotdot_escape(self):
        # A relative path can't climb out of the base via `..` and still
        # count as in-bounds.
        assert not sandbox.path_within_any(["C:/chat1"], "../outside/notes.txt")

    def test_path_within_any_case_insensitive(self):
        assert sandbox.path_within_any(["C:/Chat1"], "c:/chat1/NOTES.TXT")

    def test_extract_candidate_paths_checks_common_keys(self):
        assert sandbox.extract_candidate_paths({"path": "/a"}) == ["/a"]
        assert sandbox.extract_candidate_paths({"file_path": "/a"}) == ["/a"]
        assert sandbox.extract_candidate_paths({"paths": ["/a", "/b"]}) == ["/a"]
        assert sandbox.extract_candidate_paths({"query": "hello"}) == []

    def test_extract_shell_paths_bare_and_quoted_windows_paths(self):
        assert sandbox.extract_shell_paths(r"type C:\Users\me\a.txt") == [r"C:\Users\me\a.txt"]
        assert sandbox.extract_shell_paths('type "C:\\Users\\me\\a b.txt"') == ["C:\\Users\\me\\a b.txt"]

    def test_extract_shell_paths_no_literal_path_finds_nothing(self):
        # Documented fail-open case: a command with no literal path (e.g.
        # built from a variable) can't be caught by this heuristic at all.
        assert sandbox.extract_shell_paths("echo $SOME_VAR") == []

    def test_check_containment_no_path_args_is_trivially_in_bounds(self):
        assert sandbox.check_containment({"expression": "2+2"}, ["C:/chat1"])

    def test_check_containment_structured_path_in_and_out_of_bounds(self):
        assert sandbox.check_containment({"path": "C:/chat1/notes.txt"}, ["C:/chat1"])
        assert not sandbox.check_containment({"path": "C:/elsewhere/notes.txt"}, ["C:/chat1"])

    def test_check_containment_shell_command_out_of_bounds(self):
        assert not sandbox.check_containment(
            {"command": r"type C:\elsewhere\secret.txt"}, ["C:/chat1"]
        )

    def test_check_containment_shell_command_in_bounds(self):
        assert sandbox.check_containment(
            {"command": r'type "C:\chat1\notes.txt"'}, ["C:/chat1"]
        )

    def test_allowed_dirs_chat_mode_excludes_cwd(self):
        # Chat mode: only chat_dir + cache_dir, even if cwd somehow diverged.
        dirs = sandbox.allowed_dirs_for_session(
            {"chat_dir": "C:/chat1", "cwd": "C:/elsewhere", "mode": "chat"}, "C:/cache"
        )
        assert set(dirs) == {"C:/chat1", "C:/cache"}

    def test_allowed_dirs_agentic_mode_includes_diverged_cwd(self):
        dirs = sandbox.allowed_dirs_for_session(
            {"chat_dir": "C:/chat1", "cwd": "C:/elsewhere", "mode": "agentic"}, "C:/cache"
        )
        assert set(dirs) == {"C:/chat1", "C:/elsewhere", "C:/cache"}

    def test_allowed_dirs_agentic_mode_before_any_divergence(self):
        # cwd still equals chat_dir (no "Set as working directory" yet) —
        # listing both is harmless (same directory twice, deduped by set()).
        dirs = sandbox.allowed_dirs_for_session(
            {"chat_dir": "C:/chat1", "cwd": "C:/chat1", "mode": "agentic"}, "C:/cache"
        )
        assert set(dirs) == {"C:/chat1", "C:/cache"}

    @pytest.mark.asyncio
    async def test_run_forces_approval_for_out_of_bounds_path_despite_auto_allow(
        self, router, session, db
    ):
        # End-to-end: default_policy is auto_allow (everything would normally
        # proceed silently), but a tool call reaching outside the session's
        # chat_dir must still force a real approval prompt, never a silent
        # allow.
        provider = MockProvider("p1")
        provider.set_deltas([
            [
                Delta(tool_calls=[
                    ToolCall(id="call_1", function={
                        "name": "read_file",
                        "arguments": json.dumps({"path": "C:/somewhere/else/secret.txt"}),
                    }),
                ]),
                Delta(finish_reason="tool_calls"),
            ],
        ])
        router._providers["p1"] = provider

        await db.execute(
            "UPDATE sessions SET metadata = :meta WHERE id = :id",
            {"id": session, "meta": json.dumps({"chat_dir": "C:/chat1", "mode": "chat"})},
        )
        hitl = HITLManager(db, HITLConfig(default_policy="auto_allow"))
        context = ContextManager(db, TokenManagementConfig())
        mcp = MCPManager(db)
        agent = Agent(router, mcp, hitl, context, db)

        events = []
        async def cb(e):
            events.append(e)

        task = asyncio.create_task(agent.run(session, "hi", cb, provider_override="p1"))
        try:
            for _ in range(100):
                if any(e.type == "hitl_pause" for e in events):
                    break
                await asyncio.sleep(0.01)
            assert any(e.type == "hitl_pause" for e in events), "expected a forced approval prompt"
        finally:
            if not task.done():
                task.cancel()


# =============================================================================
#  TEST: CONTEXT MANAGER — Step 4
# =============================================================================

class TestContextManager:
    @pytest.mark.asyncio
    async def test_build_empty_session(self, context, session):
        msgs = await context.build_messages(session, "Hello", [])
        assert len(msgs) >= 2
        assert msgs[0]["role"] == "system"
        assert "helpful" in msgs[0]["content"]
        assert msgs[-1]["role"] == "user"
        assert msgs[-1]["content"] == "Hello"

    @pytest.mark.asyncio
    async def test_three_layer_prompt(self, context, session):
        tools = [ToolDefinition(name="check_weather", description="Get weather", input_schema={}, server_id="w")]
        msgs = await context.build_messages(session, "Weather?", tools, persona_override="You are a weather bot.")
        sys_msgs = [m for m in msgs if m["role"] == "system"]
        assert len(sys_msgs) >= 3
        assert any("helpful" in m["content"] for m in sys_msgs)
        assert any("weather bot" in m["content"] for m in sys_msgs)
        assert any("check_weather" in m["content"] for m in sys_msgs)

    @pytest.mark.asyncio
    async def test_no_tool_hints_without_tools(self, context, session):
        msgs = await context.build_messages(session, "Hi", [])
        tool_hints = [m for m in msgs if m["role"] == "system" and "MCP" in m["content"]]
        assert len(tool_hints) == 0

    @pytest.mark.asyncio
    async def test_persona_override(self, context, session):
        msgs = await context.build_messages(session, "Hi", [], persona_override="You are a researcher.")
        assert any("researcher" in m["content"] for m in msgs if m["role"] == "system")

    @pytest.mark.asyncio
    async def test_history_loaded_from_db(self, context, db, session):
        mid = uuid4().hex
        await db.execute(
            "INSERT INTO messages (id, session_id, role, content) VALUES (:id, :sid, 'user', :c)",
            {"id": mid, "sid": session, "c": "old message"},
        )
        msgs = await context.build_messages(session, "new msg", [])
        contents = [m.get("content", "") for m in msgs]
        assert any("old message" in c for c in contents)

    # The old in-memory, throw-away `_compact` placeholder (keep-last-4 +
    # 300-char string truncation) was replaced by the persisted, tool-aware
    # compaction subsystem in `bigtiny/agent/compaction.py` — see
    # `tests/test_compaction.py` for its coverage. `_compact` no longer
    # exists on `ContextManager`.

    @pytest.mark.asyncio
    async def test_anchor_survives_after_compaction_advances_past_it(self, context, db, session):
        # build_messages fetches the first user message via its own cheap
        # LIMIT-1 query rather than pulling it out of the (now-bounded)
        # live-tail history query — this verifies that lookup still finds
        # it and renders it as the anchor even once `compacted_through_rowid`
        # has advanced past its rowid (simulating a completed Tier-2 pass).
        cursor = await db.execute(
            "INSERT INTO messages (id, session_id, role, content) VALUES (:id, :sid, 'user', :c)",
            {"id": uuid4().hex, "sid": session, "c": "original goal: build a widget"},
        )
        first_rowid = cursor.lastrowid
        await db.execute(
            "UPDATE sessions SET compacted_through_rowid = :r WHERE id = :id",
            {"r": first_rowid, "id": session},
        )

        msgs = await context.build_messages(session, "new msg", [])
        anchors = [
            m for m in msgs
            if m["role"] == "system" and "[Original request]" in m.get("content", "")
        ]
        assert len(anchors) == 1
        assert "build a widget" in anchors[0]["content"]
        # Not duplicated into the live tail as a plain user-role message.
        assert not any(
            m["role"] == "user" and m.get("content") == "original goal: build a widget"
            for m in msgs
        )

    @pytest.mark.asyncio
    async def test_live_tail_bounded_by_compacted_through_rowid(self, context, db, session):
        rowids = []
        for i in range(5):
            role = "user" if i % 2 == 0 else "assistant"
            cursor = await db.execute(
                "INSERT INTO messages (id, session_id, role, content) VALUES (:id, :sid, :role, :c)",
                {"id": uuid4().hex, "sid": session, "role": role, "c": f"msg-{i}"},
            )
            rowids.append(cursor.lastrowid)

        # Simulate compaction having folded everything through msg-2 (index
        # 2, the third row) into the memory summary.
        await db.execute(
            "UPDATE sessions SET compacted_through_rowid = :r, "
            "memory_slots = :slots WHERE id = :id",
            {
                "r": rowids[2],
                "slots": json.dumps({
                    "new_constraints": [], "new_decisions": [], "new_completions": [],
                    "current_state": "folded msg-0..msg-2",
                }),
                "id": session,
            },
        )

        msgs = await context.build_messages(session, "new msg", [])
        contents = [m.get("content") for m in msgs]
        # msg-1/msg-2 are below (or at) the watermark — gone entirely, not
        # even in masked/summarized form as a standalone message.
        assert "msg-1" not in contents
        assert "msg-2" not in contents
        # msg-0 survives only via the anchor (it's the first user message).
        assert any(
            "msg-0" in str(c) and "[Original request]" in str(c) for c in contents
        )
        # msg-3/msg-4 are above the watermark — still present verbatim.
        assert "msg-3" in contents
        assert "msg-4" in contents

    @pytest.mark.asyncio
    async def test_count_tokens(self, context):
        msgs = [{"role": "user", "content": "Hello world, this is a test"}]
        count = await context.count_tokens(msgs)
        assert count > 0
        assert isinstance(count, int)

    @pytest.mark.asyncio
    async def test_save_messages_skips_loaded_and_system(self, context, db, session):
        msgs = [
            {"role": "system", "content": "skip"},
            {"role": "user", "content": "keep"},
        ]
        await context.save_messages(session, msgs)
        saved = await db.fetch_all(
            "SELECT * FROM messages WHERE session_id=:sid", {"sid": session},
        )
        assert len(saved) == 1
        assert saved[0]["role"] == "user"

    @pytest.mark.asyncio
    async def test_save_with_tool_calls(self, context, db, session):
        msgs = [{"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "t"}}]}]
        await context.save_messages(session, msgs)
        saved = await db.fetch_all("SELECT * FROM messages WHERE session_id=:sid", {"sid": session})
        assert len(saved) == 1
        tc = json.loads(saved[0]["tool_calls"])
        assert tc[0]["id"] == "c1"


# =============================================================================
#  TEST: AGENT LOOP — Step 4
# =============================================================================

class TestAgentLoop:
    @pytest.mark.asyncio
    async def test_session_not_found_emits_error(self, agent):
        events = []
        async def cb(e):
            events.append(e)
        await agent.run("nonexistent", "hi", cb)
        assert any(e.type == "error" for e in events)
        assert any(e.is_last for e in events)

    @pytest.mark.asyncio
    async def test_no_healthy_provider_emits_error(self, agent, session):
        events = []
        async def cb(e):
            events.append(e)
        await agent.run(session, "hi", cb)
        assert any(e.type == "error" for e in events)
        assert any(e.is_last for e in events)

    @pytest.mark.asyncio
    async def test_check_repetition_detects_3(self, agent):
        sid = uuid4().hex
        assert not agent._check_repetition(sid, "read", {"p": "/a"})
        assert not agent._check_repetition(sid, "read", {"p": "/a"})
        assert agent._check_repetition(sid, "read", {"p": "/a"})

    @pytest.mark.asyncio
    async def test_check_repetition_resets_on_diff(self, agent):
        sid = uuid4().hex
        assert not agent._check_repetition(sid, "read", {"p": "/a"})
        assert not agent._check_repetition(sid, "read", {"p": "/a"})
        assert not agent._check_repetition(sid, "read", {"p": "/b"})
        assert not agent._check_repetition(sid, "read", {"p": "/b"})
        assert agent._check_repetition(sid, "read", {"p": "/b"})

    @pytest.mark.asyncio
    async def test_check_repetition_partial_window(self, agent):
        sid = uuid4().hex
        assert not agent._check_repetition(sid, "read", {"p": "/a"})
        assert not agent._check_repetition(sid, "read", {"p": "/a"})

    @pytest.mark.asyncio
    async def test_cancel_no_task(self, agent, session):
        await agent.cancel(session)

    @pytest.mark.asyncio
    async def test_cancel_updates_session_status(self, agent, db, session):
        await agent.cancel(session)
        s = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": session})
        assert s["status"] == "idle"

    def test_dicts_to_messages(self):
        dicts = [{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello", "tool_calls": [{"id": "c1"}]}]
        msgs = _dicts_to_messages(dicts)
        assert len(msgs) == 2
        assert msgs[0].role == MessageRole.user
        assert msgs[1].tool_calls == [{"id": "c1"}]

    def test_tools_to_openai_format(self):
        tools = [ToolDefinition(name="r", description="Read", input_schema={"type": "object"}, server_id="fs")]
        result = _tools_to_openai_format(tools)
        assert result[0]["function"]["name"] == "r"
        assert result[0]["function"]["parameters"] == {"type": "object"}

    # -- title derivation: strips Kitty's hidden prompt-preamble wrappers --
    # (see `_strip_prompt_wrappers`'s doc comment) so a session's auto-title
    # reflects what the user actually typed, not "<system>" or "<recipe...>".

    def test_derive_title_plain_message(self):
        assert _derive_title("What is 2+2?") == "What is 2+2?"

    def test_derive_title_strips_system_wrapper(self):
        wrapped = "<system>\nYou are a capable assistant.\n</system>\n\nWhat is 2+2?"
        assert _derive_title(wrapped) == "What is 2+2?"

    def test_derive_title_strips_multiline_system_wrapper(self):
        wrapped = "<system>\nLine one.\nLine two.\n</system>\n\nHello there"
        assert _derive_title(wrapped) == "Hello there"

    def test_derive_title_strips_recipe_wrapper(self):
        wrapped = (
            '<recipe title="Debate">\nYou are moderating.\n</recipe>\n\n'
            "Run the recipe above now — it is mandatory for this message.\n\n"
            "Motion: X"
        )
        assert _derive_title(wrapped) == "Motion: X"

    def test_derive_title_strips_recipe_wrapping_system_wrapper(self):
        # First-turn recipe invocation: recipe wraps the system preamble.
        inner = "<system>\nYou are a capable assistant.\n</system>\n\nMotion: X"
        wrapped = f'<recipe title="Debate">\nYou are moderating.\n</recipe>\n\nRun it now.\n\n{inner}'
        assert _derive_title(wrapped) == "Motion: X"

    def test_derive_title_unwrapped_mention_of_system_tag_unaffected(self):
        text = "Can you explain what a <system> prompt is?"
        assert _derive_title(text) == text

    @pytest.mark.asyncio
    async def test_run_derives_title_stripping_system_wrapper(self, agent, router, db):
        sid = uuid4().hex
        await db.execute(
            "INSERT INTO sessions (id, name) VALUES (:id, :name)", {"id": sid, "name": None}
        )
        provider = MockProvider("p1")
        provider.set_deltas([[Delta(content="answer", finish_reason="stop")]])
        router._providers["p1"] = provider

        events = []
        async def cb(e):
            events.append(e)

        wrapped_message = "<system>\nYou are a capable assistant.\n</system>\n\nWhat is 2+2?"
        await agent.run(sid, wrapped_message, cb, provider_override="p1")

        title_events = [e for e in events if e.type == "session_title"]
        assert title_events, "expected a session_title event"
        assert title_events[0].content == "What is 2+2?"
        row = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sid})
        assert row["name"] == "What is 2+2?"

    @pytest.mark.asyncio
    async def test_run_passes_provider_temperature_and_top_p(self, agent, router, session):
        # Round-trips ProviderProfile.temperature/top_p (Kitty) -> BigTiny's
        # provider `config` column -> the actual chat_completion() call, so
        # they're no longer silently dropped (see sync_active_provider in
        # Kitty's src-tauri/src/bigtiny/providers.rs).
        provider = MockProvider("p1")
        provider.config.config = {"model": "mock-model-1", "temperature": 0.4, "top_p": 0.9}
        provider.set_deltas([[Delta(content="hi", finish_reason="stop")]])
        router._providers["p1"] = provider

        captured: dict[str, Any] = {}
        original = provider.chat_completion

        async def wrapped(messages, tools=None, **kwargs):
            captured.update(kwargs)
            async for d in original(messages, tools, **kwargs):
                yield d

        provider.chat_completion = wrapped

        events = []
        async def cb(e):
            events.append(e)

        await agent.run(session, "hello", cb, provider_override="p1")

        assert captured.get("temperature") == 0.4
        assert captured.get("top_p") == 0.9

    @pytest.mark.asyncio
    async def test_run_uses_provider_context_length_for_compaction_threshold(
        self, router, mcp, hitl, db, session
    ):
        # A provider's context_length should widen (or narrow) the point at
        # which BigTiny starts summarizing old history for that provider,
        # rather than being a fully inert field.
        provider = MockProvider("p1")
        provider.config.config = {"model": "mock-model-1", "context_length": 500}
        provider.set_deltas([[Delta(content="hi", finish_reason="stop")]])
        router._providers["p1"] = provider

        tiny_token_config = TokenManagementConfig(max_context_tokens=100000, compaction_threshold=0.8)
        context = ContextManager(db, tiny_token_config)
        agent = Agent(router, mcp, hitl, context, db)

        captured: dict[str, Any] = {}
        original_build = context.build_messages

        async def wrapped_build(*args, **kwargs):
            captured.update(kwargs)
            return await original_build(*args, **kwargs)

        context.build_messages = wrapped_build

        events = []
        async def cb(e):
            events.append(e)

        await agent.run(session, "hello", cb, provider_override="p1")

        assert captured.get("max_context_tokens_override") == 500

    @pytest.mark.asyncio
    async def test_run_executes_multiple_tool_calls_concurrently(self, router, session, db):
        # Two tool calls in one turn, each simulating 0.2s of work — if they
        # ran sequentially (the old behavior) the turn would take >=0.4s;
        # concurrent execution should finish well under that.
        provider = MockProvider("p1")
        provider.set_deltas([
            [
                Delta(tool_calls=[
                    ToolCall(id="call_1", function={"name": "slow_a", "arguments": "{}"}),
                    ToolCall(id="call_2", function={"name": "slow_b", "arguments": "{}"}),
                ]),
                Delta(finish_reason="tool_calls"),
            ],
            [Delta(content="done", finish_reason="stop")],
        ])
        router._providers["p1"] = provider

        hitl_config = HITLConfig(default_policy="auto_allow")
        hitl = HITLManager(db, hitl_config)
        context = ContextManager(db, TokenManagementConfig())

        class SlowMCP:
            async def list_tools(self):
                return []

            async def execute_tool(self, tool_name, args, timeout=30):
                await asyncio.sleep(0.2)
                return ToolResult(
                    content=f"{tool_name}-done",
                    tool_call_id=f"{tool_name}_x",
                    duration_ms=200,
                    output_size_bytes=0,
                    is_error=False,
                )

        agent = Agent(router, SlowMCP(), hitl, context, db)

        captured_messages: list[dict] = []
        original_save = context.save_messages

        async def wrapped_save(sid, messages):
            captured_messages.extend(messages)
            return await original_save(sid, messages)

        context.save_messages = wrapped_save

        events = []
        async def cb(e):
            events.append(e)

        start = time.monotonic()
        await agent.run(session, "hi", cb, provider_override="p1")
        elapsed = time.monotonic() - start

        assert elapsed < 0.35, f"expected concurrent execution, took {elapsed}s"

        tool_messages = [m for m in captured_messages if m["role"] == "tool"]
        by_id = {m["tool_call_id"]: m["content"] for m in tool_messages}
        assert by_id == {"call_1": "slow_a-done", "call_2": "slow_b-done"}

    @pytest.mark.asyncio
    async def test_concurrent_pending_approvals_resolve_independently(self, agent, router, hitl, session):
        # Two tool calls in one turn both need approval (default_policy is
        # always_ask) — before the action_id-keying fix, `_hitl_events` was
        # keyed by session_id, so the second call's wait would clobber the
        # first's dict entry. Confirms both get independent, correctly-keyed
        # waits, and resolving one doesn't disturb the other.
        provider = MockProvider("p1")
        provider.set_deltas([
            [
                Delta(tool_calls=[
                    ToolCall(id="call_1", function={"name": "tool_a", "arguments": "{}"}),
                    ToolCall(id="call_2", function={"name": "tool_b", "arguments": "{}"}),
                ]),
                Delta(finish_reason="tool_calls"),
            ],
            [Delta(content="done", finish_reason="stop")],
        ])
        router._providers["p1"] = provider

        events = []
        async def cb(e):
            events.append(e)

        task = asyncio.create_task(agent.run(session, "hi", cb, provider_override="p1"))
        try:
            for _ in range(100):
                if len(hitl.get_pending_approvals(session)) == 2:
                    break
                await asyncio.sleep(0.01)
            pending = hitl.get_pending_approvals(session)
            assert len(pending) == 2
            action_ids = {p.action_id for p in pending}
            assert set(agent._hitl_events.keys()) == action_ids

            first, second = pending[0].action_id, pending[1].action_id
            await hitl.record_decision(first, "allow")
            agent._hitl_events[first].set()
            await asyncio.sleep(0.02)
            # The second action's wait must be untouched by resolving the first.
            assert second in agent._hitl_events
            assert not agent._hitl_events[second].is_set()

            await hitl.record_decision(second, "allow")
            agent._hitl_events[second].set()
            await asyncio.wait_for(task, timeout=2)
        finally:
            if not task.done():
                task.cancel()

    @pytest.mark.asyncio
    async def test_run_persists_messages_incrementally_not_only_at_the_end(
        self, router, db, session
    ):
        # Regression coverage: `save_messages` used to be called exactly once,
        # after the whole turn finished — meaning a window that switched away
        # mid-turn and back saw nothing new at all (not even the user's own
        # just-sent message) until the entire turn completed. Confirms
        # multiple incremental saves happen, and that the final persisted
        # history has no duplicates (which a buggy `last_saved` slice
        # bookkeeping would produce).
        provider = MockProvider("p1")
        provider.set_deltas([
            [
                Delta(tool_calls=[
                    ToolCall(id="call_1", function={"name": "tool_a", "arguments": "{}"}),
                ]),
                Delta(finish_reason="tool_calls"),
            ],
            [Delta(content="done", finish_reason="stop")],
        ])
        router._providers["p1"] = provider

        hitl_config = HITLConfig(default_policy="auto_allow")
        hitl2 = HITLManager(db, hitl_config)
        context = ContextManager(db, TokenManagementConfig())

        class FastMCP:
            async def list_tools(self):
                return []

            async def execute_tool(self, tool_name, args, timeout=30):
                return ToolResult(
                    content="tool-a-done", tool_call_id="x", duration_ms=1,
                    output_size_bytes=0, is_error=False,
                )

        agent = Agent(router, FastMCP(), hitl2, context, db)

        save_call_sizes: list[int] = []
        original_save = context.save_messages

        async def wrapped_save(sid, msgs):
            save_call_sizes.append(len(msgs))
            return await original_save(sid, msgs)

        context.save_messages = wrapped_save

        events = []
        async def cb(e):
            events.append(e)

        await agent.run(session, "hi", cb, provider_override="p1")

        # More than the single end-of-turn call this used to be, and every
        # call actually persisted something new (save_new_messages skips a
        # call entirely when there's nothing new, so a zero-size entry here
        # would itself be a bug).
        assert len(save_call_sizes) >= 3
        assert all(n > 0 for n in save_call_sizes)

        rows = await db.fetch_all(
            "SELECT role FROM messages WHERE session_id = :sid ORDER BY created_at ASC, rowid ASC",
            {"sid": session},
        )
        # Exactly one row per turn-message, in order, with no duplicates —
        # would fail if the incremental slicing re-saved an already-persisted
        # message.
        assert [r["role"] for r in rows] == ["user", "assistant", "tool", "assistant"]

    @pytest.mark.asyncio
    async def test_run_persists_completed_rounds_before_a_later_cancellation(
        self, router, db, session
    ):
        # A cancelled/interrupted run must not lose an already-completed
        # tool round just because the turn never reached its normal
        # end-of-loop save point.
        class HangingProvider(Provider):
            def __init__(self):
                super().__init__(
                    "p1",
                    ProviderConfig(
                        id="p1", name="p1", provider_type=ProviderType.openai_compat,
                        base_url="http://localhost:8000",
                    ),
                )
                self._call = 0

            async def chat_completion(self, messages, tools=None, **kwargs):
                self._call += 1
                if self._call == 1:
                    yield Delta(tool_calls=[
                        ToolCall(id="call_1", function={"name": "tool_a", "arguments": "{}"}),
                    ])
                    yield Delta(finish_reason="tool_calls")
                else:
                    # Never resolves — the test cancels the run while this
                    # second LLM call is in flight, simulating a switch-away
                    # (or any other cancellation) mid-turn.
                    await asyncio.Event().wait()
                    yield Delta(content="unreachable", finish_reason="stop")  # pragma: no cover

            async def discover_models(self):
                return []

            async def count_tokens(self, messages):
                return 0

            async def check_health(self):
                return HealthStatus(status="healthy")

        provider = HangingProvider()
        router._providers["p1"] = provider

        hitl_config = HITLConfig(default_policy="auto_allow")
        hitl2 = HITLManager(db, hitl_config)
        context = ContextManager(db, TokenManagementConfig())

        class FastMCP:
            async def list_tools(self):
                return []

            async def execute_tool(self, tool_name, args, timeout=30):
                return ToolResult(
                    content="tool-a-done", tool_call_id="x", duration_ms=1,
                    output_size_bytes=0, is_error=False,
                )

        agent = Agent(router, FastMCP(), hitl2, context, db)
        events = []
        async def cb(e):
            events.append(e)

        task = asyncio.create_task(agent.run(session, "hi", cb, provider_override="p1"))
        try:
            for _ in range(200):
                rows = await db.fetch_all(
                    "SELECT role FROM messages WHERE session_id = :sid", {"sid": session}
                )
                if len(rows) >= 3:  # user, assistant(tool_calls), tool
                    break
                await asyncio.sleep(0.01)
            else:
                pytest.fail("first tool round never persisted before timing out")

            task.cancel()
            await asyncio.wait_for(task, timeout=2)  # run() swallows CancelledError itself

            rows = await db.fetch_all(
                "SELECT role FROM messages WHERE session_id = :sid "
                "ORDER BY created_at ASC, rowid ASC",
                {"sid": session},
            )
            assert [r["role"] for r in rows] == ["user", "assistant", "tool"]
        finally:
            if not task.done():
                task.cancel()


# =============================================================================
#  TEST: SSE EVENTS — Step 5
# =============================================================================

class TestSSEEvents:
    def test_all_types_serialize(self):
        types = ["llm_delta", "llm_stop", "tool_start", "tool_finish",
                 "hitl_pause", "hitl_resolved", "error", "model_failover",
                 "subagent_status", "session_status"]
        for t in types:
            e = SSEEvent(type=t, session_id="s1", is_last=True)
            s = serialize_sse(e)
            assert s.startswith("data: ")
            assert s.endswith("\n\n")
            parsed = json.loads(s[6:].strip())
            assert parsed["type"] == t

    def test_is_last_field_present(self):
        e = SSEEvent(type="session_status", session_id="s1", is_last=True)
        s = serialize_sse(e)
        parsed = json.loads(s[6:].strip())
        assert parsed["is_last"] is True

    def test_none_fields_serialized(self):
        e = SSEEvent(type="llm_delta", session_id="s1")
        s = serialize_sse(e)
        parsed = json.loads(s[6:].strip())
        assert parsed["content"] is None

    def test_complex_nested_dict(self):
        e = SSEEvent(type="tool_start", tool_name="bash", tool_args={"cmd": "ls -la", "cwd": "/tmp"}, session_id="s1")
        s = serialize_sse(e)
        parsed = json.loads(s[6:].strip())
        assert parsed["tool_name"] == "bash"
        assert parsed["tool_args"]["cmd"] == "ls -la"

    def test_error_fields_included(self):
        e = SSEEvent(type="error", error_code="PROVIDER_DOWN", error_message="Connection refused", session_id="s1", is_last=True)
        s = serialize_sse(e)
        parsed = json.loads(s[6:].strip())
        assert parsed["error_code"] == "PROVIDER_DOWN"
        assert parsed["recoverable"] is True


# =============================================================================
#  TEST: MIDDLEWARE — Step 5
# =============================================================================

class TestMiddleware:
    def test_cors_applied(self):
        from fastapi import FastAPI
        app = FastAPI()
        add_middleware(app)
        cls_names = [str(m.cls) for m in app.user_middleware]
        assert any("CORSMiddleware" in c for c in cls_names)

    def test_middleware_counts(self):
        from fastapi import FastAPI
        app = FastAPI()
        add_middleware(app)
        assert len(app.user_middleware) >= 2


# =============================================================================
#  TEST: RECIPE ENGINE — Step 7
# =============================================================================

class TestRecipeEngine:
    @pytest.mark.asyncio
    async def test_execute_not_found(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        with pytest.raises(ValueError, match="nonexistent"):
            await engine.execute("nonexistent", {})

    @pytest.mark.asyncio
    async def test_execute_creates_session(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "test_recipe", "prompt": "Hello {{name}}"},
        )
        events = []
        async def cb(e):
            events.append(e)
        sid = await engine.execute(rid, {"name": "World"}, cb)
        assert sid is not None
        session = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sid})
        assert session is not None
        assert session["name"] == "test_recipe"

    @pytest.mark.asyncio
    async def test_execute_stores_metadata(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "test", "prompt": "test"},
        )
        events = []
        async def cb(e):
            events.append(e)
        sid = await engine.execute(rid, {"key": "val"}, cb)
        session = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sid})
        meta = json.loads(session["metadata"])
        assert meta["recipe_id"] == rid
        assert meta["parameters"] == {"key": "val"}

    @pytest.mark.asyncio
    async def test_load_yaml_from_directory(self, agent, mcp, db):
        with tempfile.TemporaryDirectory() as tmp:
            recipe_path = Path(tmp) / "test.yaml"
            recipe_path.write_text("name: yaml_recipe\nprompt_template: 'Research {{topic}}'\n")
            engine = RecipeEngine(db, agent, mcp, recipes_dir=tmp)
            count = await engine.load_recipes_from_directory()
            assert count == 1
            recipe = await db.fetch_one("SELECT * FROM recipes WHERE name=:name", {"name": "yaml_recipe"})
            assert recipe is not None

    @pytest.mark.asyncio
    async def test_load_nonexistent_directory(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp, recipes_dir="/nonexistent_dir_xyz")
        count = await engine.load_recipes_from_directory()
        assert count == 0

    @pytest.mark.asyncio
    async def test_load_invalid_yaml_skipped(self, agent, mcp, db):
        with tempfile.TemporaryDirectory() as tmp:
            bad = Path(tmp) / "bad.yaml"
            bad.write_text("not: yaml: : : {")
            engine = RecipeEngine(db, agent, mcp, recipes_dir=tmp)
            count = await engine.load_recipes_from_directory()
            assert count == 0


# =============================================================================
#  TEST: SCHEDULER — Step 7
# =============================================================================

class TestScheduler:
    @pytest.mark.asyncio
    async def test_add_job_returns_id(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        sched = Scheduler(db, engine)
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "t", "prompt": "test"},
        )
        jid = await sched.add_job(JobConfig(name="j1", cron="0 0 * * *", recipe_id=rid))
        assert jid is not None
        assert len(jid) == 8

    @pytest.mark.asyncio
    async def test_add_job_persists(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        sched = Scheduler(db, engine)
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "t", "prompt": "test"},
        )
        jid = await sched.add_job(JobConfig(name="j2", cron="*/5 * * * *", recipe_id=rid))
        row = await db.fetch_one("SELECT * FROM schedule_jobs WHERE id=:id", {"id": jid})
        assert row is not None
        assert row["cron"] == "*/5 * * * *"

    @pytest.mark.asyncio
    async def test_run_job_not_found(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        sched = Scheduler(db, engine)
        with pytest.raises(ValueError):
            await sched.run_job("nonexistent")

    @pytest.mark.asyncio
    async def test_start_stop_cycle(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        sched = Scheduler(db, engine)
        await sched.start()
        await sched.stop()

    @pytest.mark.asyncio
    async def test_execute_job_logs_history(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        sched = Scheduler(db, engine)
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "t", "prompt": "test"},
        )
        jid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO schedule_jobs (id, name, cron, recipe_id, parameters) VALUES (:id, :n, :c, :rid, :p)",
            {"id": jid, "n": "j3", "c": "0 0 * * *", "rid": rid, "p": "{}"},
        )
        await sched._execute_job(jid)
        row = await db.fetch_one(
            "SELECT * FROM execution_history WHERE trigger_id=:tid", {"tid": jid},
        )
        assert row is not None

    @pytest.mark.asyncio
    async def test_execute_job_failure_marks_temp_session_failed_not_deleted(
        self, agent, mcp, db
    ):
        # `execution_history.session_id` is NOT NULL + REFERENCES sessions(id)
        # with foreign_keys=ON and no ON DELETE clause — deleting the temp
        # session on the failure path (as the success path does) would raise
        # a FOREIGN KEY constraint violation, since the execution_history
        # row for this failed run still points at it. The fix marks the temp
        # session `status='failed'` instead of deleting it, and leaves the
        # execution_history audit row intact — this verifies both the DB
        # stays internally consistent (no FK error escapes `_execute_job`)
        # and the "failed" marker is applied rather than left at 'idle'.
        engine = RecipeEngine(db, agent, mcp)
        sched = Scheduler(db, engine)
        job_id = uuid4().hex[:8]
        # `schedule_jobs.recipe_id` has its own FK to `recipes(id)`, so the
        # recipe row must exist to insert the job at all — the failure
        # instead comes from a malformed prompt template, which
        # `recipe_engine.execute` fails to compile (jinja2 raises at
        # `from_string`), landing in `_execute_job`'s except branch.
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :n, :p)",
            {"id": rid, "n": "broken", "p": "{{ unterminated"},
        )
        await db.execute(
            "INSERT INTO schedule_jobs (id, name, cron, recipe_id, parameters) "
            "VALUES (:id, :n, :c, :rid, :p)",
            {"id": job_id, "n": "will_fail", "c": "0 0 * * *", "rid": rid, "p": "{}"},
        )

        await sched._execute_job(job_id)  # must not raise

        history = await db.fetch_one(
            "SELECT * FROM execution_history WHERE trigger_id = :tid", {"tid": job_id}
        )
        assert history is not None
        assert history["status"] == "failed"

        temp_session = await db.fetch_one(
            "SELECT * FROM sessions WHERE id = :id", {"id": history["session_id"]}
        )
        assert temp_session is not None
        assert temp_session["status"] == "failed"


# =============================================================================
#  TEST: SUBAGENT — Step 9
# =============================================================================

class TestSubagent:
    @pytest.mark.asyncio
    async def test_spawn_returns_id(self, agent, db, session):
        mgr = SubagentManager(agent, db)
        sid = await mgr.spawn(session, "do something")
        assert sid.startswith("sub_")
        assert len(sid) > 4

    @pytest.mark.asyncio
    async def test_spawn_creates_child_session(self, agent, db, session):
        mgr = SubagentManager(agent, db)
        sid = await mgr.spawn(session, "test")
        sub = mgr.get_subagent(sid)
        assert sub is not None
        child = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sub.session_id})
        assert child is not None
        assert child["name"] == f"subagent_{sid}"

    @pytest.mark.asyncio
    async def test_spawn_sets_parent_metadata(self, agent, db, session):
        mgr = SubagentManager(agent, db)
        sid = await mgr.spawn(session, "test")
        sub = mgr.get_subagent(sid)
        assert sub is not None
        child = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sub.session_id})
        meta = json.loads(child["metadata"])
        assert meta["parent_session"] == session

    @pytest.mark.asyncio
    async def test_get_unknown_returns_none(self, agent, db):
        mgr = SubagentManager(agent, db)
        assert mgr.get_subagent("nonexistent") is None

    @pytest.mark.asyncio
    async def test_list_subagents_by_parent(self, agent, db, session):
        mgr = SubagentManager(agent, db)
        await mgr.spawn(session, "a")
        await mgr.spawn(session, "b")
        children = mgr.list_subagents(session)
        assert len(children) == 2

    @pytest.mark.asyncio
    async def test_spawn_status_is_running(self, agent, db, session):
        mgr = SubagentManager(agent, db)
        sid = await mgr.spawn(session, "test")
        sub = mgr.get_subagent(sid)
        assert sub is not None
        assert sub.status == "running"

    @pytest.mark.asyncio
    async def test_wait_for_nonexistent(self, agent, db):
        mgr = SubagentManager(agent, db)
        result = await mgr.wait_for_completion("nonexistent")
        assert result is None

    @pytest.mark.asyncio
    async def test_result_is_full_joined_content_not_just_last_chunk(
        self, router, mcp, hitl, context, db, session
    ):
        # Regression test: `_run_subagent` used to scan its buffered events
        # backwards for the *last* `llm_delta` and take only that single
        # chunk's own `.content` as the result — for any streamed reply
        # spanning more than one delta, `subagent.result` was silently just
        # the final fragment. Accumulating chunks into a list and joining
        # fixes this as a side effect of removing the full-event buffer.
        provider = MockProvider("p1")
        provider.set_deltas([[
            Delta(content="Hello "),
            Delta(content="there, "),
            Delta(content="world!"),
            Delta(finish_reason="stop"),
        ]])
        router._providers["p1"] = provider

        agent = Agent(router, mcp, hitl, context, db)
        mgr = SubagentManager(agent, db)
        sid = await mgr.spawn(session, "say hi", provider_override="p1")
        sub = await mgr.wait_for_completion(sid, timeout=5)

        assert sub is not None
        assert sub.status == "completed"
        assert sub.result == "Hello there, world!"

    @pytest.mark.asyncio
    async def test_completed_subagents_are_evicted_after_retention_window(
        self, agent, db, session
    ):
        mgr = SubagentManager(agent, db)
        sid = await mgr.spawn(session, "test")
        old = await mgr.wait_for_completion(sid, timeout=5)
        assert old is not None
        # Back-date completion so the next spawn's sweep considers it stale
        # (mirrors HITLManager's sweep-on-access pattern — no dedicated
        # background task needed).
        from bigtiny.subagent import manager as subagent_module
        old.completed_at = datetime.utcnow() - subagent_module.SUBAGENT_RETENTION - timedelta(minutes=1)

        await mgr.spawn(session, "trigger sweep")

        assert mgr.get_subagent(sid) is None


# =============================================================================
#  TEST: SESSION STATS — Step 9
# =============================================================================

class TestSessionStats:
    @pytest.mark.asyncio
    async def test_get_stats_empty(self, stats, session):
        result = await stats.get_stats(session)
        assert result["message_count"] == 0
        assert result["tokens_sent"] == 0
        assert result["tokens_received"] == 0

    @pytest.mark.asyncio
    async def test_get_stats_with_messages(self, stats, db, session):
        mid = uuid4().hex
        await db.execute(
            "INSERT INTO messages (id, session_id, role, content, token_count) "
            "VALUES (:id, :sid, 'user', :c, 10)",
            {"id": mid, "sid": session, "c": "test"},
        )
        mid2 = uuid4().hex
        await db.execute(
            "INSERT INTO messages (id, session_id, role, content, token_count) "
            "VALUES (:id, :sid, 'assistant', :c, 20)",
            {"id": mid2, "sid": session, "c": "response"},
        )
        result = await stats.get_stats(session)
        assert result["message_count"] == 2
        assert result["tokens_sent"] == 10
        assert result["tokens_received"] == 20

    @pytest.mark.asyncio
    async def test_record_usage(self, stats, db, session):
        await stats.record_usage(session, 100, 50, "openai", "gpt-4")
        await stats.record_usage(session, 200, 100, "anthropic", "claude-3")
        result = await stats.get_stats(session)
        assert len(result["provider_history"]) == 2
        assert result["provider_history"][0]["provider"] == "openai"

    @pytest.mark.asyncio
    async def test_record_usage_capped(self, stats, db, session):
        for i in range(105):
            await stats.record_usage(session, i, 0, "test", "m")
        result = await stats.get_stats(session)
        assert len(result["provider_history"]) == 100

    @pytest.mark.asyncio
    async def test_get_stats_unknown_session(self, stats):
        result = await stats.get_stats("nonexistent")
        assert result["session_id"] == "nonexistent"
        assert result["message_count"] == 0

    @pytest.mark.asyncio
    async def test_estimated_cost(self, stats, db, session):
        mid = uuid4().hex
        await db.execute(
            "INSERT INTO messages (id, session_id, role, content, token_count) "
            "VALUES (:id, :sid, 'user', :c, 1000)",
            {"id": mid, "sid": session, "c": "x" * 100},
        )
        result = await stats.get_stats(session)
        assert isinstance(result["estimated_cost_usd"], (int, float))


# =============================================================================
#  TEST: DISCOVERY — Step 10
# =============================================================================

class TestDiscovery:
    @pytest.mark.asyncio
    async def test_discover_all_empty(self, db, router):
        disc = LocalModelDiscovery(db, router)
        models = await disc.discover_all()
        assert models == []

    @pytest.mark.asyncio
    async def test_discover_provider_not_found(self, db, router):
        disc = LocalModelDiscovery(db, router)
        models = await disc.discover_provider("nonexistent")
        assert models == []

    @pytest.mark.asyncio
    async def test_cache_invalidate_single(self, db, router):
        disc = LocalModelDiscovery(db, router)
        disc._cache["p1"] = (datetime.utcnow(), [ModelInfo(id="m1")])
        disc.invalidate_cache("p1")
        assert "p1" not in disc._cache

    @pytest.mark.asyncio
    async def test_cache_invalidate_all(self, db, router):
        disc = LocalModelDiscovery(db, router)
        disc._cache["p1"] = (datetime.utcnow(), [])
        disc._cache["p2"] = (datetime.utcnow(), [])
        disc.invalidate_cache()
        assert len(disc._cache) == 0

    @pytest.mark.asyncio
    async def test_cache_ttl_hit(self, db, router):
        disc = LocalModelDiscovery(db, router)
        now = datetime.utcnow()
        disc._cache["p1"] = (now, [ModelInfo(id="m1")])
        result = await disc._discover_provider("p1")
        assert len(result) == 1

    @pytest.mark.asyncio
    async def test_discover_provider_bypasses_cache(self, db, router):
        disc = LocalModelDiscovery(db, router)
        disc._cache["p1"] = (datetime.utcnow(), [ModelInfo(id="cached")])
        result = await disc.discover_provider("p1")
        # Since there's no actual provider in router, should return []
        assert result == []


# =============================================================================
#  INTEGRATION TESTS — Routes
# =============================================================================

class TestIntegrationRoutes:
    @pytest_asyncio.fixture
    async def test_app(self, db, config):
        from fastapi import FastAPI
        from bigtiny.server.routes.chat import router as chat_router
        from bigtiny.server.routes.health import router as health_router
        from bigtiny.server.routes.providers import router as providers_router
        from bigtiny.server.routes.mcp import router as mcp_router
        from bigtiny.server.routes.recipes import router as recipes_router
        from bigtiny.server.routes.schedules import router as schedules_router

        app = FastAPI()
        app.state.db = db
        app.state.agent = Agent(
            ProviderRouter(db), MCPManager(db),
            HITLManager(db, HITLConfig()),
            ContextManager(db, TokenManagementConfig()),
            db,
        )
        app.state.mcp = MCPManager(db)
        app.state.router = ProviderRouter(db)
        app.state.hitl = HITLManager(db, HITLConfig())
        app.state.recipe_engine = RecipeEngine(db, app.state.agent, app.state.mcp)
        app.state.scheduler = Scheduler(db, app.state.recipe_engine)
        app.state.config = config
        app.state.startup_time = time.time()

        app.include_router(chat_router)
        app.include_router(health_router)
        app.include_router(providers_router)
        app.include_router(mcp_router)
        app.include_router(recipes_router)
        app.include_router(schedules_router)
        return app

    # --- Chat ---
    @pytest.mark.asyncio
    async def test_chat_create_session(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.post("/api/chat/", params={"name": "test_sess"})
            assert resp.status_code == 200
            data = resp.json()
            assert "session_id" in data

    @pytest.mark.asyncio
    async def test_chat_list_sessions(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            await ac.post("/api/chat/", params={"name": "s1"})
            resp = await ac.get("/api/chat/")
            assert resp.status_code == 200
            data = resp.json()
            assert data["total"] >= 1

    @pytest.mark.asyncio
    async def test_chat_delete_session(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            create = await ac.post("/api/chat/")
            sid = create.json()["session_id"]
            resp = await ac.delete(f"/api/chat/{sid}")
            assert resp.status_code == 200
            assert resp.json()["status"] == "deleted"

    @pytest.mark.asyncio
    async def test_chat_get_stats(self, test_app, db):
        from httpx import AsyncClient, ASGITransport
        sid = uuid4().hex
        await db.execute("INSERT INTO sessions (id, name) VALUES (:id, :name)", {"id": sid, "name": "t"})
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.get(f"/api/chat/{sid}/stats")
            assert resp.status_code == 200
            data = resp.json()
            assert data["session_id"] == sid

    @pytest.mark.asyncio
    async def test_chat_get_history(self, test_app, db):
        from httpx import AsyncClient, ASGITransport
        sid = uuid4().hex
        await db.execute("INSERT INTO sessions (id, name) VALUES (:id, :name)", {"id": sid, "name": "t"})
        mid = uuid4().hex
        await db.execute("INSERT INTO messages (id, session_id, role, content) VALUES (:id, :sid, 'user', :c)",
                         {"id": mid, "sid": sid, "c": "hello"})
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.get(f"/api/chat/{sid}/history")
            assert resp.status_code == 200
            data = resp.json()
            assert len(data) >= 1

    @pytest.mark.asyncio
    async def test_chat_get_pending(self, test_app):
        from httpx import AsyncClient, ASGITransport
        sid = uuid4().hex
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.get(f"/api/chat/{sid}/pending")
            assert resp.status_code == 200

    # --- Health ---
    @pytest.mark.asyncio
    async def test_health(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.get("/api/health")
            assert resp.status_code == 200
            data = resp.json()
            assert data["status"] == "healthy"

    @pytest.mark.asyncio
    async def test_status(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.get("/api/status")
            assert resp.status_code == 200

    # --- Providers ---
    @pytest.mark.asyncio
    async def test_providers_crud(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            create = await ac.post("/api/providers", json={
                "name": "test_prov", "provider_type": "openai_compat",
                "base_url": "http://localhost:8000",
            })
            assert create.status_code == 200
            pid = create.json()["id"]

            lst = await ac.get("/api/providers")
            assert lst.status_code == 200
            assert len(lst.json()["providers"]) >= 1

            patch = await ac.patch(f"/api/providers/{pid}", json={"name": "updated"})
            assert patch.status_code == 200

    @pytest.mark.asyncio
    async def test_providers_test_not_found(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.post("/api/providers/nonexistent/test")
            assert resp.status_code == 404

    @pytest.mark.asyncio
    async def test_providers_models_not_found(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.get("/api/providers/nonexistent/models")
            assert resp.status_code == 404

    # --- MCP ---
    @pytest.mark.asyncio
    async def test_mcp_crud(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            create = await ac.post("/api/mcp/servers", json={
                "name": "test_mcp", "transport": "stdio", "command": "python",
            })
            assert create.status_code == 200
            sid = create.json()["id"]

            lst = await ac.get("/api/mcp/servers")
            assert lst.status_code == 200
            assert len(lst.json()["servers"]) >= 1

            tools = await ac.get(f"/api/mcp/servers/{sid}/tools")
            assert tools.status_code == 200

    # --- Recipes ---
    @pytest.mark.asyncio
    async def test_recipes_crud(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            create = await ac.post("/api/recipes", json={
                "name": "test_recipe", "prompt_template": "Hello {{name}}",
            })
            assert create.status_code == 200
            rid = create.json()["id"]

            lst = await ac.get("/api/recipes")
            assert lst.status_code == 200

            delete = await ac.delete(f"/api/recipes/{rid}")
            assert delete.status_code == 200

    @pytest.mark.asyncio
    async def test_recipes_execute_not_found(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.post("/api/recipes/nonexistent/execute", json={"parameters": {}})
            assert resp.status_code == 404

    @pytest.mark.asyncio
    async def test_recipes_delete_not_found(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.delete("/api/recipes/nonexistent")
            assert resp.status_code == 404

    # --- Schedules ---
    @pytest.mark.asyncio
    async def test_schedules_crud(self, test_app, db):
        from httpx import AsyncClient, ASGITransport
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "t", "prompt": "test"},
        )
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            create = await ac.post("/api/schedules", json={
                "name": "test_sched", "cron": "0 0 * * *", "recipe_id": rid,
            })
            assert create.status_code == 200
            jid = create.json()["id"]

            lst = await ac.get("/api/schedules")
            assert lst.status_code == 200

            run = await ac.post(f"/api/schedules/{jid}/run_now")
            assert run.status_code == 200

            patch = await ac.patch(f"/api/schedules/{jid}", json={"name": "updated"})
            assert patch.status_code == 200

    @pytest.mark.asyncio
    async def test_schedules_run_now_not_found(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.post("/api/schedules/nonexistent/run_now")
            assert resp.status_code == 404

    @pytest.mark.asyncio
    async def test_schedules_patch_not_found(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.patch("/api/schedules/nonexistent", json={"name": "x"})
            assert resp.status_code == 404

    @pytest.mark.asyncio
    async def test_schedules_delete_not_found(self, test_app):
        from httpx import AsyncClient, ASGITransport
        transport = ASGITransport(app=test_app)
        async with AsyncClient(transport=transport, base_url="http://test") as ac:
            resp = await ac.delete("/api/schedules/nonexistent")
            assert resp.status_code == 404


# =============================================================================
#  ROBUSTNESS TESTS
# =============================================================================

class TestRobustness:
    @pytest.mark.asyncio
    async def test_concurrent_db_access(self, db):
        async def writer():
            for i in range(20):
                sid = uuid4().hex
                await db.execute(
                    "INSERT INTO sessions (id, name) VALUES (:id, :name)",
                    {"id": sid, "name": f"w{i}"},
                )
                await asyncio.sleep(0)

        async def reader():
            for i in range(20):
                rows = await db.fetch_all("SELECT * FROM sessions")
                assert isinstance(rows, list)
                await asyncio.sleep(0)

        await asyncio.gather(writer(), reader())

    @pytest.mark.asyncio
    async def test_context_1000_messages(self, context, db, session):
        for i in range(100):
            uid = uuid4().hex
            await db.execute(
                "INSERT INTO messages (id, session_id, role, content) VALUES (:id, :sid, 'user', :c)",
                {"id": uid, "sid": session, "c": f"user msg {i}"},
            )
            aid = uuid4().hex
            await db.execute(
                "INSERT INTO messages (id, session_id, role, content) VALUES (:id, :sid, 'assistant', :c)",
                {"id": aid, "sid": session, "c": f"asst response {i}"},
            )
        msgs = await context.build_messages(session, "final", [])
        assert len(msgs) > 0
        assert msgs[-1]["content"] == "final"

    @pytest.mark.asyncio
    async def test_empty_provider_router(self, router):
        with pytest.raises(NoHealthyProvider):
            await router.get_provider()

    @pytest.mark.asyncio
    async def test_mcp_execute_empty_args(self, mcp):
        result = await mcp.execute_tool("nonexistent", {})
        assert result.is_error

    @pytest.mark.asyncio
    async def test_hitl_reject_checks_all_patterns(self, hitl, session):
        for cmd in ["rm -rf /important", "chmod 777 /etc", "dd if=/dev/zero of=/dev/sda", "mkfs.ext4 /dev/sda"]:
            result = await hitl.check_tool_call(session, "bash", {"command": cmd})
            assert result.action == "rejected", f"Pattern should reject: {cmd}"

    @pytest.mark.asyncio
    async def test_recipe_empty_parameters(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        rid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "t", "prompt": "no placeholders"},
        )
        events = []
        async def cb(e):
            events.append(e)
        sid = await engine.execute(rid, {}, cb)
        assert sid is not None

    @pytest.mark.asyncio
    async def test_subagent_multiple_spawns(self, agent, db, session):
        mgr = SubagentManager(agent, db)
        ids = []
        for i in range(5):
            sid = await mgr.spawn(session, f"task {i}")
            ids.append(sid)
        assert len(ids) == 5
        assert len(set(ids)) == 5

    @pytest.mark.asyncio
    async def test_stats_record_usage_no_session(self, stats):
        await stats.record_usage("nonexistent", 10, 5, "test", "m")

    @pytest.mark.asyncio
    async def test_scheduler_execute_with_bad_template(self, agent, mcp, db):
        engine = RecipeEngine(db, agent, mcp)
        sched = Scheduler(db, engine)
        rid = uuid4().hex[:8]
        jid = uuid4().hex[:8]
        await db.execute(
            "INSERT INTO recipes (id, name, prompt_template) VALUES (:id, :name, :prompt)",
            {"id": rid, "name": "bad", "prompt": "{%"},
        )
        await db.execute(
            "INSERT INTO schedule_jobs (id, name, cron, recipe_id, parameters) VALUES (:id, :n, :c, :rid, :p)",
            {"id": jid, "n": "bad_job", "c": "0 0 * * *", "rid": rid, "p": "{}"},
        )
        await sched._execute_job(jid)
        row = await db.fetch_one(
            "SELECT * FROM execution_history WHERE trigger_id=:tid", {"tid": jid},
        )
        assert row is not None
        assert row["status"] == "failed"

    @pytest.mark.asyncio
    async def test_openai_timeout_params(self):
        from bigtiny.providers.openai_compat import OpenAICompatibleProvider
        from bigtiny.models.provider import ProviderConfig, ProviderType
        prov = OpenAICompatibleProvider("t", ProviderConfig(id="t", name="t", provider_type=ProviderType.openai_compat, base_url="http://localhost"))
        t = prov._client.timeout
        assert t.connect == 3.0
        assert t.read == 60.0

    @pytest.mark.asyncio
    async def test_anthropic_timeout_params(self):
        from bigtiny.providers.anthropic import AnthropicProvider
        from bigtiny.models.provider import ProviderConfig, ProviderType
        prov = AnthropicProvider("t", ProviderConfig(id="t", name="t", provider_type=ProviderType.anthropic, base_url="https://api.anthropic.com"))
        t = prov._client.timeout
        assert t.connect == 5.0
        assert t.read == 120.0
