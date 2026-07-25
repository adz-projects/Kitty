"""Tests for the Kitty-facing chat-first features (Workstream A):
auth, images, reasoning/usage, fork, per-session config, titles, cwd.
"""

from __future__ import annotations

import asyncio
import json
from uuid import uuid4

import httpx
import pytest
import pytest_asyncio

from bigtiny.storage import Database
from bigtiny.config import TokenManagementConfig
from bigtiny.models.session import Message, MessageRole
from bigtiny.models.provider import ProviderConfig, ProviderType
from bigtiny.agent.context_manager import ContextManager, SessionStats
from bigtiny.agent.loop import _derive_title


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
def token_config():
    return TokenManagementConfig()


def _openai_prov():
    from bigtiny.providers.openai_compat import OpenAICompatibleProvider
    return OpenAICompatibleProvider("t", ProviderConfig(
        id="t", name="t", provider_type=ProviderType.openai_compat,
        base_url="http://localhost"))


def _anthropic_prov():
    from bigtiny.providers.anthropic import AnthropicProvider
    return AnthropicProvider("t", ProviderConfig(
        id="t", name="t", provider_type=ProviderType.anthropic,
        base_url="https://api.anthropic.com"))


class TestImageBlocks:
    def test_openai_image_serialization(self):
        prov = _openai_prov()
        msg = Message(session_id="s", role=MessageRole.user, content=[
            {"type": "text", "text": "what is this?"},
            {"type": "image", "data": "QUJD", "mime_type": "image/jpeg"},
        ])
        result = prov._serialize_message(msg)
        assert result["content"][0] == {"type": "text", "text": "what is this?"}
        assert result["content"][1]["type"] == "image_url"
        assert result["content"][1]["image_url"]["url"] == "data:image/jpeg;base64,QUJD"

    def test_anthropic_image_serialization(self):
        prov = _anthropic_prov()
        msg = Message(session_id="s", role=MessageRole.user, content=[
            {"type": "text", "text": "what is this?"},
            {"type": "image", "data": "QUJD", "mime_type": "image/jpeg"},
        ])
        result = prov._serialize_message(msg)
        assert result["content"][1]["type"] == "image"
        assert result["content"][1]["source"] == {
            "type": "base64", "media_type": "image/jpeg", "data": "QUJD"}

    @pytest.mark.asyncio
    async def test_blocks_survive_save_and_load(self, db, token_config):
        sid = uuid4().hex
        await db.execute("INSERT INTO sessions (id) VALUES (:id)", {"id": sid})
        cm = ContextManager(db, token_config)
        blocks = [{"type": "text", "text": "hi"},
                  {"type": "image", "data": "QUJD", "mime_type": "image/png"}]
        await cm.save_messages(sid, [{"role": "user", "content": blocks}])
        msgs = await cm.build_messages(sid, "next", [])
        loaded = [m for m in msgs if isinstance(m.get("content"), list)]
        assert loaded and loaded[0]["content"] == blocks

    @pytest.mark.asyncio
    async def test_build_messages_with_images(self, db, token_config):
        sid = uuid4().hex
        await db.execute("INSERT INTO sessions (id) VALUES (:id)", {"id": sid})
        cm = ContextManager(db, token_config)
        msgs = await cm.build_messages(
            sid, "look", [], images=[{"data": "QUJD", "mime_type": "image/png"}])
        user_msg = msgs[-1]
        assert isinstance(user_msg["content"], list)
        assert user_msg["content"][0] == {"type": "text", "text": "look"}
        assert user_msg["content"][1]["type"] == "image"


class TestReasoningAndUsage:
    def test_openai_reasoning_content(self):
        prov = _openai_prov()
        data = '{"choices":[{"delta":{"reasoning_content":"thinking..."},"finish_reason":null}]}'
        delta = prov._parse_chunk(data, {})
        assert delta is not None
        assert delta.reasoning == "thinking..."
        assert delta.content is None

    def test_openai_usage_chunk_without_choices(self):
        prov = _openai_prov()
        data = '{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}'
        delta = prov._parse_chunk(data, {})
        assert delta is not None
        assert delta.usage == {"input_tokens": 10, "output_tokens": 5}

    def test_anthropic_thinking_delta(self):
        prov = _anthropic_prov()
        delta = prov._parse_event({"type": "content_block_delta",
                                   "delta": {"type": "thinking_delta",
                                             "thinking": "hmm"}})
        assert delta is not None
        assert delta.reasoning == "hmm"

    def test_resolve_model(self):
        prov = _openai_prov()
        assert prov.resolve_model() == "gpt-4o"
        assert prov.resolve_model("llama3") == "llama3"
        prov.config.config = {"model": "qwen"}
        assert prov.resolve_model() == "qwen"
        assert prov.resolve_model("llama3") == "llama3"

    @pytest.mark.asyncio
    async def test_record_usage_roundtrip(self, db):
        sid = uuid4().hex
        await db.execute("INSERT INTO sessions (id) VALUES (:id)", {"id": sid})
        stats = SessionStats(db)
        await stats.record_usage(sid, 100, 20, "prov1", "gpt-4o")
        result = await stats.get_stats(sid)
        assert result["provider_history"] == [{
            "prompt_tokens": 100, "completion_tokens": 20,
            "provider": "prov1", "model": "gpt-4o"}]


class TestProviderConfigThreading:
    """The config column (carrying "model") must reach ProviderConfig — it was
    silently dropped by ProviderRouter._instantiate, so every registered
    provider fell back to DEFAULT_MODEL (found live: Ollama 404 on gpt-4o)."""

    def test_instantiate_parses_json_config_string(self):
        from bigtiny.providers.router import ProviderRouter
        router = ProviderRouter(None)
        row = {"id": "p", "name": "n", "provider_type": "openai_compat",
               "base_url": "http://localhost:11434",
               "config": '{"model": "qwen"}'}
        prov = router._instantiate(row, None)
        assert prov.resolve_model() == "qwen"

    def test_instantiate_accepts_dict_config(self):
        from bigtiny.providers.router import ProviderRouter
        router = ProviderRouter(None)
        row = {"id": "p", "name": "n", "provider_type": "openai_compat",
               "base_url": "http://localhost:11434",
               "config": {"model": "llama3"}}
        prov = router._instantiate(row, None)
        assert prov.resolve_model() == "llama3"

    def test_instantiate_tolerates_missing_or_bad_config(self):
        from bigtiny.providers.router import ProviderRouter
        router = ProviderRouter(None)
        base = {"id": "p", "name": "n", "provider_type": "openai_compat",
                "base_url": "http://localhost:11434"}
        assert router._instantiate(dict(base), None).resolve_model() == "gpt-4o"
        assert router._instantiate({**base, "config": "not json"}, None) \
            .resolve_model() == "gpt-4o"


class TestHitlPauseActionId:
    def test_sse_event_serializes_action_id(self):
        from bigtiny.server.events import SSEEvent, serialize_sse
        wire = serialize_sse(SSEEvent(type="hitl_pause", action_id="act1"))
        assert '"action_id": "act1"' in wire


class TestSessionTitle:
    def test_derive_title_short(self):
        assert _derive_title("Hello world") == "Hello world"

    def test_derive_title_first_line_and_truncation(self):
        assert _derive_title("first line\nsecond line") == "first line"
        long_text = "word " * 30
        t = _derive_title(long_text)
        assert len(t) <= 61
        assert t.endswith("…")

    def test_derive_title_empty(self):
        assert _derive_title("   ") == ""


class TestChatRoutesWorkstreamA:
    """Route-level tests via the FastAPI app with a wired test state."""

    def _mk_app(self, db):
        from fastapi import FastAPI
        from bigtiny.server.routes.chat import router as chat_router
        app = FastAPI()
        app.include_router(chat_router)
        app.state.db = db
        return app

    @pytest.mark.asyncio
    async def test_create_session_with_cwd(self, db):
        app = self._mk_app(db)
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
            r = await client.post("/api/chat/", json={"name": "n1", "cwd": "C:/work"})
            sid = r.json()["session_id"]
        row = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sid})
        assert row["name"] == "n1"
        assert json.loads(row["metadata"]) == {"cwd": "C:/work"}

    @pytest.mark.asyncio
    async def test_fork_copies_and_truncates(self, db):
        sid = uuid4().hex
        await db.execute(
            "INSERT INTO sessions (id, name) VALUES (:id, :n)",
            {"id": sid, "n": "orig"})
        for i, (role, text) in enumerate([("user", "q1"), ("assistant", "a1"),
                                          ("user", "q2"), ("assistant", "a2")]):
            await db.execute(
                "INSERT INTO messages (id, session_id, role, content) "
                "VALUES (:id, :sid, :r, :c)",
                {"id": f"m{i}", "sid": sid, "r": role, "c": text})
        app = self._mk_app(db)
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
            r = await client.post(f"/api/chat/{sid}/fork", json={"at_message_id": "m1"})
            body = r.json()
            assert body["copied_messages"] == 2
            new_sid = body["session_id"]
            r2 = await client.post(f"/api/chat/{sid}/fork", json={})
            assert r2.json()["copied_messages"] == 4
            r3 = await client.post(f"/api/chat/{sid}/fork", json={"at_message_id": "nope"})
            assert r3.status_code == 404
        copied = await db.fetch_all(
            "SELECT * FROM messages WHERE session_id=:sid ORDER BY created_at, rowid",
            {"sid": new_sid})
        assert [m["content"] for m in copied] == ["q1", "a1"]
        new_session = await db.fetch_one(
            "SELECT * FROM sessions WHERE id=:id", {"id": new_sid})
        assert new_session["name"] == "orig (branch)"
        assert json.loads(new_session["metadata"])["forked_from"] == sid

    @pytest.mark.asyncio
    async def test_session_config_patch(self, db):
        sid = uuid4().hex
        await db.execute("INSERT INTO sessions (id) VALUES (:id)", {"id": sid})
        app = self._mk_app(db)
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
            r = await client.patch(f"/api/chat/{sid}/config",
                                   json={"provider": "p1", "model": "m1"})
            assert r.json()["config"]["model"] == "m1"
            r = await client.patch(f"/api/chat/{sid}/config", json={"model": ""})
            assert r.json()["config"]["model"] is None
            assert r.json()["config"]["provider"] == "p1"
        row = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sid})
        meta = json.loads(row["metadata"])
        assert meta.get("provider") == "p1"
        assert "model" not in meta

    @pytest.mark.asyncio
    async def test_rename_session(self, db):
        sid = uuid4().hex
        await db.execute("INSERT INTO sessions (id) VALUES (:id)", {"id": sid})
        app = self._mk_app(db)
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
            r = await client.patch(f"/api/chat/{sid}", json={"name": "renamed"})
            assert r.status_code == 200
            r404 = await client.patch(f"/api/chat/{uuid4().hex}", json={"name": "x"})
            assert r404.status_code == 404
        row = await db.fetch_one("SELECT * FROM sessions WHERE id=:id", {"id": sid})
        assert row["name"] == "renamed"


class TestAPIKeyMiddleware:
    def _mk_app(self, secret):
        from fastapi import FastAPI
        from bigtiny.server.middleware import APIKeyMiddleware
        app = FastAPI()

        @app.get("/api/health")
        async def health():
            return {"ok": True}

        @app.get("/api/chat/")
        async def chat():
            return {"ok": True}

        app.add_middleware(APIKeyMiddleware, secret=secret)
        return app

    @pytest.mark.asyncio
    async def test_secret_enforced(self):
        transport = httpx.ASGITransport(app=self._mk_app("s3cret"))
        async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
            assert (await client.get("/api/chat/")).status_code == 401
            wrong = await client.get("/api/chat/", headers={"X-API-Key": "wrong"})
            assert wrong.status_code == 401
            ok = await client.get("/api/chat/", headers={"X-API-Key": "s3cret"})
            assert ok.status_code == 200
            # health stays open for readiness polling
            assert (await client.get("/api/health")).status_code == 200

    @pytest.mark.asyncio
    async def test_no_secret_open(self):
        transport = httpx.ASGITransport(app=self._mk_app(None))
        async with httpx.AsyncClient(transport=transport, base_url="http://t") as client:
            assert (await client.get("/api/chat/")).status_code == 200
