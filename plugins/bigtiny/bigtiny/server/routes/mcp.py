from __future__ import annotations

import json
import logging
from uuid import uuid4

from fastapi import APIRouter, HTTPException, Request
from pydantic import BaseModel

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/api/mcp")


class CreateMCPServerRequest(BaseModel):
    name: str
    transport: str  # "stdio" | "sse" | "streamable_http"
    command: str | None = None
    args: list[str] | None = None
    url: str | None = None
    env: dict[str, str] | None = None
    headers: dict[str, str] | None = None
    enabled: bool = True


class UpdateMCPServerRequest(BaseModel):
    name: str | None = None
    transport: str | None = None
    command: str | None = None
    args: list[str] | None = None
    url: str | None = None
    env: dict[str, str] | None = None
    headers: dict[str, str] | None = None
    enabled: bool | None = None


@router.get("/servers")
async def list_mcp_servers(request: Request):
    db = request.app.state.db
    rows = await db.fetch_all("SELECT * FROM mcp_servers")
    return {"servers": [dict(r) for r in rows]}


@router.post("/servers")
async def add_mcp_server(body: CreateMCPServerRequest, request: Request):
    db = request.app.state.db
    server_id = uuid4().hex[:8]
    await db.execute(
        "INSERT INTO mcp_servers (id, name, transport, command, args, url, env, headers, enabled) "
        "VALUES (:id, :name, :transport, :command, :args, :url, :env, :headers, :enabled)",
        {
            "id": server_id,
            "name": body.name,
            "transport": body.transport,
            "command": body.command,
            "args": json.dumps(body.args or []),
            "url": body.url,
            "env": json.dumps(body.env or {}),
            "headers": json.dumps(body.headers) if body.headers else None,
            "enabled": 1 if body.enabled else 0,
        },
    )
    return {"id": server_id, "status": "created"}


@router.patch("/servers/{server_id}")
async def update_mcp_server(server_id: str, body: UpdateMCPServerRequest, request: Request):
    db = request.app.state.db
    mcp = request.app.state.mcp

    row = await db.fetch_one("SELECT * FROM mcp_servers WHERE id = :id", {"id": server_id})
    if not row:
        raise HTTPException(404, detail=f"MCP server not found: {server_id}")

    updates = body.model_dump(exclude_unset=True)
    if not updates:
        return dict(row)

    config_fields_changed = any(
        field in updates for field in ("transport", "command", "args", "url", "env", "headers")
    )

    set_clauses = []
    params: dict[str, object] = {"id": server_id}
    for field, value in updates.items():
        if field == "args" or field == "env":
            params[field] = json.dumps(value)
        elif field == "headers":
            params[field] = json.dumps(value) if value else None
        elif field == "enabled":
            params[field] = 1 if value else 0
        else:
            params[field] = value
        set_clauses.append(f"{field} = :{field}")
    set_clauses.append("updated_at = CURRENT_TIMESTAMP")

    await db.execute(
        f"UPDATE mcp_servers SET {', '.join(set_clauses)} WHERE id = :id",
        params,
    )

    was_connected = row["status"] == "connected"
    will_be_enabled = updates.get("enabled", bool(row.get("enabled", 1)))

    if was_connected and (config_fields_changed or not will_be_enabled):
        try:
            await mcp.disconnect_server(server_id)
        except Exception:
            logger.exception("Failed to disconnect MCP server %s during update", server_id)

    if will_be_enabled and (config_fields_changed or not was_connected):
        try:
            await mcp.connect_server(server_id)
        except Exception as e:
            logger.warning("Failed to reconnect MCP server %s after update: %s", server_id, e)

    fresh = await db.fetch_one("SELECT * FROM mcp_servers WHERE id = :id", {"id": server_id})
    return dict(fresh)


@router.post("/servers/{server_id}/connect")
async def connect_mcp_server(server_id: str, request: Request):
    mcp = request.app.state.mcp
    try:
        status = await mcp.connect_server(server_id)
        return {"status": status}
    except Exception as e:
        logger.exception("Failed to connect MCP server %s", server_id)
        raise HTTPException(400, detail=str(e) or repr(e))


@router.get("/servers/{server_id}/tools")
async def list_mcp_tools(server_id: str, request: Request):
    mcp = request.app.state.mcp
    tools = await mcp.list_tools(server_id)
    return {"tools": [t.model_dump() for t in tools]}


@router.delete("/servers/{server_id}")
async def delete_mcp_server(server_id: str, request: Request):
    mcp = request.app.state.mcp
    db = request.app.state.db
    try:
        await mcp.disconnect_server(server_id)
    except Exception:
        pass
    await db.execute("DELETE FROM mcp_servers WHERE id = :id", {"id": server_id})
    return {"status": "deleted"}
