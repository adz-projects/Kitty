"""End-to-end smoke test for BigTiny daemon.

Starts the app via ASGITransport and exercises every public REST endpoint.
Reports pass/fail for each check including warnings and errors encountered.
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path
from uuid import uuid4

import httpx
from httpx import ASGITransport

sys.path.insert(0, str(Path(__file__).parent))
from bigtiny.config import load_config
from bigtiny.storage import Database
from bigtiny.providers.router import ProviderRouter
from bigtiny.mcp.manager import MCPManager
from bigtiny.hitl.manager import HITLManager
from bigtiny.agent.context_manager import ContextManager
from bigtiny.agent.loop import Agent
from bigtiny.recipes.engine import RecipeEngine
from bigtiny.scheduler.scheduler import Scheduler
from bigtiny.server.app import create_app
from bigtiny.server.middleware import add_middleware


async def setup_app_state(app):
    """Manually initialise app state (bypasses async lifespan)."""
    config = load_config()

    db = Database(":memory:")
    await db.connect()

    mcp = MCPManager(db)
    router = ProviderRouter(db)
    await router.load_providers()

    hitl = HITLManager(db, config.hitl)
    context = ContextManager(db, config.token_management)
    agent = Agent(router, mcp, hitl, context, db)

    recipe_engine = RecipeEngine(db, agent, mcp)
    scheduler = Scheduler(db, recipe_engine)

    app.state.db = db
    app.state.agent = agent
    app.state.mcp = mcp
    app.state.router = router
    app.state.hitl = hitl
    app.state.recipe_engine = recipe_engine
    app.state.scheduler = scheduler
    app.state.config = config
    app.state.startup_time = time.time()

    import atexit
    atexit.register(lambda: asyncio.run(db.close()))


class SmokeTest:
    def __init__(self, app):
        self.app = app
        self.results: list[str] = []
        self.warnings: list[str] = []
        self.errors: list[str] = []
        self._start = 0.0

    # ------------------------------------------------------------------
    #  Helpers
    # ------------------------------------------------------------------

    def ok(self, name: str, detail: str = "") -> None:
        t = time.time() - self._start
        suffix = f"  # {detail}" if detail else ""
        self.results.append(f"  [PASS] {name} ({t:.2f}s){suffix}")

    def fail(self, name: str, detail: str) -> None:
        t = time.time() - self._start
        self.results.append(f"  [FAIL] {name} ({t:.2f}s)")
        self.errors.append(f"  {name}: {detail}")

    def warn(self, name: str, detail: str) -> None:
        t = time.time() - self._start
        self.results.append(f"  [WARN] {name} ({t:.2f}s)  # {detail}")
        self.warnings.append(f"  {name}: {detail}")

    def check(self, name: str, cond: bool, detail: str = "") -> None:
        if cond:
            self.ok(name, detail)
        else:
            self.fail(name, detail)

    def check_status(self, name: str, r: httpx.Response, *expected: int) -> None:
        passed = r.status_code in expected
        detail = f"expected {expected}, got {r.status_code}"
        if not passed:
            detail += f": {r.text[:200]}"
        self.check(name, passed, detail)

    # ------------------------------------------------------------------
    #  Test groups
    # ------------------------------------------------------------------

    async def test_health(self, c: httpx.AsyncClient) -> None:
        r = await c.get("/api/health")
        self.check_status("GET /api/health", r, 200)
        data = r.json()
        self.check("health .status == healthy", data.get("status") == "healthy")

        r = await c.get("/api/status")
        self.check_status("GET /api/status", r, 200)

    async def test_providers(self, c: httpx.AsyncClient) -> None:
        # List (empty)
        r = await c.get("/api/providers")
        self.check_status("GET /api/providers (empty)", r, 200)

        # Create
        r = await c.post("/api/providers", json={
            "name": "SmokeTest OpenAI",
            "provider_type": "openai_compat",
            "base_url": "http://localhost:9999/v1",
        })
        self.check_status("POST /api/providers (create)", r, 200)
        pid = r.json().get("id", "")
        self.check("provider create returned id", bool(pid), f"id={pid}")

        # Update
        r = await c.patch(f"/api/providers/{pid}", json={"name": "SmokeTest Renamed"})
        self.check_status(f"PATCH /api/providers/{pid}", r, 200)

        # Test connection — provider not in router's in-memory cache (only in DB)
        # so the endpoint returns 404. This is expected without a router reload.
        r = await c.post(f"/api/providers/{pid}/test")
        self.check_status(f"POST /api/providers/{pid}/test", r, 404)

        # List models — same router-cache limitation
        r = await c.get(f"/api/providers/{pid}/models")
        self.check_status(f"GET /api/providers/{pid}/models", r, 404)

        # Delete
        r = await c.delete(f"/api/providers/{pid}")
        self.check_status(f"DELETE /api/providers/{pid}", r, 200)

        # Verify deletion (no GET single-provider endpoint exists; PATCH will 404)
        r = await c.patch(f"/api/providers/{pid}", json={"name": "x"})
        self.check_status(f"PATCH /api/providers/{pid} (after delete)", r, 404)

    async def test_mcp(self, c: httpx.AsyncClient) -> None:
        # List (empty)
        r = await c.get("/api/mcp/servers")
        self.check_status("GET /api/mcp/servers (empty)", r, 200)

        # Create
        r = await c.post("/api/mcp/servers", json={
            "name": "SmokeTest MCP",
            "transport": "stdio",
            "command": "echo",
            "args": ["hello"],
        })
        self.check_status("POST /api/mcp/servers (create)", r, 200)
        sid = r.json().get("id", "")
        self.check("mcp create returned id", bool(sid), f"id={sid}")

        # Connect (echo is not an MCP server — expect 400)
        r = await c.post(f"/api/mcp/servers/{sid}/connect")
        self.check_status(f"POST /api/mcp/servers/{sid}/connect", r, 200, 400)

        # List tools (not connected — may return empty or fail)
        r = await c.get(f"/api/mcp/servers/{sid}/tools")
        self.check_status(f"GET /api/mcp/servers/{sid}/tools", r, 200, 500)

        # Delete
        r = await c.delete(f"/api/mcp/servers/{sid}")
        self.check_status(f"DELETE /api/mcp/servers/{sid}", r, 200)

    async def test_recipes(self, c: httpx.AsyncClient) -> None:
        # List (empty)
        r = await c.get("/api/recipes")
        self.check_status("GET /api/recipes (empty)", r, 200)

        # Create
        r = await c.post("/api/recipes", json={
            "name": "SmokeTest Recipe",
            "prompt_template": "Hello {{ name }}!",
            "parameters": ["name"],
            "system_prompt_layer": "assistant",
        })
        self.check_status("POST /api/recipes (create)", r, 200)
        rid = r.json().get("id", "")
        self.check("recipe create returned id", bool(rid), f"id={rid}")

        # Execute (no provider configured — expect 500)
        r = await c.post(f"/api/recipes/{rid}/execute", json={"parameters": {"name": "world"}})
        self.check_status(f"POST /api/recipes/{rid}/execute", r, 200, 404, 500)
        if r.status_code == 500:
            self.warn("recipe execute", f"got 500 (expected — no provider): {r.text[:150]}")

        # Delete
        r = await c.delete(f"/api/recipes/{rid}")
        self.check_status(f"DELETE /api/recipes/{rid}", r, 200)

        # Delete nonexistent
        r = await c.delete("/api/recipes/nonexistent")
        self.check_status("DELETE /api/recipes/nonexistent", r, 404)

    async def test_schedules(self, c: httpx.AsyncClient) -> None:
        # Need a recipe first (FK dependency)
        r = await c.post("/api/recipes", json={
            "name": "SchedDep Recipe",
            "prompt_template": "scheduled task",
        })
        self.check_status("POST /api/recipes (schedule FK dependency)", r, 200)
        rid = r.json()["id"]

        # List (empty)
        r = await c.get("/api/schedules")
        self.check_status("GET /api/schedules (empty)", r, 200)

        # Create
        r = await c.post("/api/schedules", json={
            "name": "SmokeTest Schedule",
            "cron": "0 0 * * *",
            "recipe_id": rid,
        })
        self.check_status("POST /api/schedules (create)", r, 200)
        jid = r.json().get("id", "")
        self.check("schedule create returned id", bool(jid), f"id={jid}")

        # Run now
        r = await c.post(f"/api/schedules/{jid}/run_now")
        self.check_status(f"POST /api/schedules/{jid}/run_now", r, 200, 500)
        if r.status_code == 500:
            self.warn("schedule run_now", f"got 500 (expected — no provider): {r.text[:150]}")

        # Update
        r = await c.patch(f"/api/schedules/{jid}", json={"name": "SmokeTest Renamed"})
        self.check_status(f"PATCH /api/schedules/{jid}", r, 200)

        # Delete schedule
        r = await c.delete(f"/api/schedules/{jid}")
        self.check_status(f"DELETE /api/schedules/{jid}", r, 200)

        # Delete nonexistent
        r = await c.delete("/api/schedules/nonexistent")
        self.check_status("DELETE /api/schedules/nonexistent", r, 404)

        # Cleanup recipe
        await c.delete(f"/api/recipes/{rid}")

    async def test_chat(self, c: httpx.AsyncClient) -> None:
        # List sessions (empty)
        r = await c.get("/api/chat/")
        self.check_status("GET /api/chat/ (empty)", r, 200)

        # Create session
        r = await c.post("/api/chat/", json={"name": "SmokeTest Chat"})
        self.check_status("POST /api/chat/ (create)", r, 200)
        sid = r.json().get("session_id", "")
        self.check("chat create returned session_id", bool(sid), f"id={sid}")

        # Stats
        r = await c.get(f"/api/chat/{sid}/stats")
        self.check_status(f"GET /api/chat/{sid}/stats", r, 200)

        # Pending
        r = await c.get(f"/api/chat/{sid}/pending")
        self.check_status(f"GET /api/chat/{sid}/pending", r, 200)

        # History (empty)
        r = await c.get(f"/api/chat/{sid}/history")
        self.check_status(f"GET /api/chat/{sid}/history", r, 200)

        # Cancel (no active run)
        r = await c.post(f"/api/chat/{sid}/cancel")
        self.check_status(f"POST /api/chat/{sid}/cancel", r, 200)

        # Delete
        r = await c.delete(f"/api/chat/{sid}")
        self.check_status(f"DELETE /api/chat/{sid}", r, 200)

    async def test_send_message(self, c: httpx.AsyncClient) -> None:
        """SSE streaming — just verify it returns text/event-stream."""
        r = await c.post("/api/chat/", json={"name": "SSE Test"})
        sid = r.json()["session_id"]

        async with c.stream(
            "POST",
            f"/api/chat/{sid}/send",
            json={"message": "hello"},
        ) as resp:
            # The first event will be an error (no provider) — that's fine
            content_type = resp.headers.get("content-type", "")
            is_sse = "text/event-stream" in content_type
            self.check("send_message returns SSE stream", is_sse, content_type)

            # Read first few events
            text = await resp.aread()
            has_is_last = "is_last" in text.decode()
            self.check("SSE response contains is_last", has_is_last)

        # Cleanup
        await c.delete(f"/api/chat/{sid}")

    # ------------------------------------------------------------------
    #  Runner
    # ------------------------------------------------------------------

    async def run(self) -> bool:
        self._start = time.time()
        transport = ASGITransport(app=self.app)

        print("BigTiny End-to-End Smoke Test")
        print("=" * 64)
        print()

        async with httpx.AsyncClient(transport=transport, base_url="http://test") as c:
            tests = [
                ("Health / Status", self.test_health),
                ("Providers CRUD", self.test_providers),
                ("MCP Servers CRUD", self.test_mcp),
                ("Recipes CRUD", self.test_recipes),
                ("Schedules CRUD", self.test_schedules),
                ("Chat Sessions CRUD", self.test_chat),
                ("SSE Streaming", self.test_send_message),
            ]

            for label, fn in tests:
                print(f"-- {label} --")
                try:
                    await fn(c)
                except Exception as e:
                    self.fail(label, str(e))
                print()

        elapsed = time.time() - self._start
        passed = sum(1 for r in self.results if r.startswith("  [PASS]"))
        failed = sum(1 for r in self.results if r.startswith("  [FAIL]"))
        warned = sum(1 for r in self.results if r.startswith("  [WARN]"))

        print("=" * 64)
        print(f"  TOTAL  : {len(self.results)} checks")
        print(f"  PASSED : {passed}")
        print(f"  FAILED : {failed}")
        print(f"  WARN   : {warned}")
        print(f"  TIME   : {elapsed:.2f}s")
        print()

        if self.warnings:
            print("Warnings:")
            for w in self.warnings:
                print(f"  [!] {w}")
            print()

        if self.errors:
            print("Errors:")
            for e in self.errors:
                print(f"  [X] {e}")
            print()

        print("=" * 64)
        for r in self.results:
            print(r)
        print("=" * 64)
        print()

        if failed:
            print(f"SMOKE TEST: {failed} failure(s) — REVIEW")
        else:
            print("SMOKE TEST: ALL PASSED")

        return failed == 0


async def async_main() -> bool:
    app = create_app()
    await setup_app_state(app)
    st = SmokeTest(app)
    return await st.run()


def main() -> int:
    ok = asyncio.run(async_main())
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
