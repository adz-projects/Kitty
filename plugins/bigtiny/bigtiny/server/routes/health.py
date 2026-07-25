from __future__ import annotations

import time

from fastapi import APIRouter, Request

from bigtiny.agent.loop import Agent
from bigtiny.providers.router import ProviderRouter
from bigtiny.mcp.manager import MCPManager
from bigtiny.storage import Database

router = APIRouter()


@router.get("/api/health")
async def health_check(request: Request):
    router: ProviderRouter = request.app.state.router
    mcp: MCPManager = request.app.state.mcp
    db: Database = request.app.state.db
    agent: Agent = request.app.state.agent
    startup_time: float = request.app.state.startup_time

    provider_health = await router.check_all_health()
    mcp_status: dict[str, str] = {}
    for sid, client in mcp._servers.items():
        mcp_status[sid] = "connected"

    return {
        "status": "healthy",
        "providers": {
            pid: {"status": h.status, "latency_ms": h.latency_ms}
            for pid, h in provider_health.items()
        },
        "mcp_servers": mcp_status,
        "uptime_sec": int(time.time() - startup_time),
        "active_sessions": len(agent._tasks),
    }


@router.get("/api/status")
async def detailed_status(request: Request):
    db: Database = request.app.state.db
    agent: Agent = request.app.state.agent
    mcp: MCPManager = request.app.state.mcp
    router: ProviderRouter = request.app.state.router

    total_sessions = await db.fetch_one("SELECT COUNT(*) as count FROM sessions")
    provider_health = await router.check_all_health()

    return {
        "sessions": (total_sessions["count"] if total_sessions else 0),
        "active_sessions": len(agent._tasks),
        "providers": list(router.get_provider_ids()),
        "mcp_servers": list(mcp._servers.keys()),
        "provider_health": {
            pid: {"status": h.status, "latency_ms": h.latency_ms}
            for pid, h in provider_health.items()
        },
    }
