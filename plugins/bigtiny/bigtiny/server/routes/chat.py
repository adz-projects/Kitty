from __future__ import annotations

import asyncio
import json
import logging
from uuid import uuid4

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import StreamingResponse
from pydantic import BaseModel

from bigtiny.agent.loop import Agent
from bigtiny.server.events import SSEEvent, serialize_sse

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/api/chat")


class ImageAttachment(BaseModel):
    data: str  # base64
    mime_type: str = "image/png"


class SendMessageRequest(BaseModel):
    message: str
    images: list[ImageAttachment] | None = None


class ApproveRequest(BaseModel):
    action_id: str
    decision: str


class CreateSessionRequest(BaseModel):
    name: str | None = None
    cwd: str | None = None
    # "chat" | "agentic" (matches Kitty's own `modeOverride` vocabulary
    # verbatim — no translation layer) — governs directory-sandboxing scope
    # (see `bigtiny/agent/sandbox.py`); defaults to "chat" when omitted.
    mode: str | None = None


class ForkRequest(BaseModel):
    # copy messages up to and including this message id; omit to copy all
    at_message_id: str | None = None


class SessionConfigRequest(BaseModel):
    provider: str | None = None
    model: str | None = None
    persona_override: str | None = None
    # "chat" | "agentic" — see `CreateSessionRequest.mode`.
    mode: str | None = None
    # Repoints the session's *current* working directory (agentic mode only
    # — see `update_session_config`). Never touches `chat_dir`, the
    # original, immutable directory set once at creation.
    cwd: str | None = None


class RenameSessionRequest(BaseModel):
    name: str


@router.post("/{session_id}/send")
async def send_message(session_id: str, body: SendMessageRequest, request: Request):
    agent: Agent = request.app.state.agent
    # Bounded so a slow/stalled SSE consumer applies natural backpressure
    # (the producer's `await queue.put(event)` briefly waits) instead of
    # this queue growing without bound in memory — nothing here needs
    # fire-and-forget semantics, and a healthy consumer never gets close to
    # this size in normal operation.
    queue: asyncio.Queue[SSEEvent] = asyncio.Queue(maxsize=1000)

    async def callback(event: SSEEvent):
        await queue.put(event)

    images = (
        [{"data": i.data, "mime_type": i.mime_type} for i in body.images]
        if body.images
        else None
    )
    task = asyncio.create_task(
        agent.run(session_id, body.message, callback, images=images)
    )
    agent._tasks[session_id] = task

    async def event_generator():
        try:
            while True:
                try:
                    event = await asyncio.wait_for(queue.get(), timeout=300)
                except asyncio.TimeoutError:
                    yield serialize_sse(SSEEvent(
                        type="error",
                        error_message="Agent timed out",
                        session_id=session_id,
                        is_last=True,
                    ))
                    break

                yield serialize_sse(event)

                if event.is_last:
                    break
        finally:
            if not task.done():
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
            agent._tasks.pop(session_id, None)

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )


@router.get("/{session_id}/pending")
async def get_pending(session_id: str, request: Request):
    hitl = request.app.state.hitl
    pending = hitl.get_pending_approvals(session_id)
    return [p.to_dict() for p in pending]


@router.post("/{session_id}/approve")
async def approve_action(session_id: str, body: ApproveRequest, request: Request):
    hitl = request.app.state.hitl
    agent: Agent = request.app.state.agent

    decision = await hitl.record_decision(body.action_id, body.decision)

    # Keyed by action_id, not session_id — a session can have more than one
    # tool call pending approval at once (see `Agent._hitl_events`).
    event = agent._hitl_events.get(body.action_id)
    if event:
        event.set()

    return {"status": "approved" if decision.action == "proceed" else "rejected", "decision": decision.action}


@router.post("/{session_id}/cancel")
async def cancel_session(session_id: str, request: Request):
    agent: Agent = request.app.state.agent
    await agent.cancel(session_id)
    return {"status": "cancelled"}


@router.get("/{session_id}/history")
async def get_history(session_id: str, request: Request, limit: int = 50):
    db = request.app.state.db
    messages = await db.fetch_all(
        "SELECT * FROM messages WHERE session_id = :sid "
        "ORDER BY created_at DESC, rowid DESC LIMIT :limit",
        {"sid": session_id, "limit": limit},
    )
    return [dict(m) for m in reversed(messages)]


@router.get("/{session_id}/stats")
async def get_stats(session_id: str, request: Request):
    db = request.app.state.db
    messages = await db.fetch_all(
        "SELECT * FROM messages WHERE session_id = :sid ORDER BY created_at ASC",
        {"sid": session_id},
    )
    total_tokens = sum(m.get("token_count", 0) for m in messages)
    return {
        "session_id": session_id,
        "message_count": len(messages),
        "total_tokens": total_tokens,
    }


@router.get("/")
async def list_sessions(request: Request, limit: int = 50, offset: int = 0):
    db = request.app.state.db
    sessions = await db.fetch_all(
        "SELECT * FROM sessions ORDER BY updated_at DESC LIMIT :limit OFFSET :offset",
        {"limit": limit, "offset": offset},
    )
    total = await db.fetch_one("SELECT COUNT(*) as count FROM sessions")
    return {"sessions": [dict(s) for s in sessions], "total": total["count"] if total else 0}


@router.post("/")
async def create_session(
    request: Request,
    body: CreateSessionRequest | None = None,
    name: str | None = None,
):
    db = request.app.state.db
    session_id = uuid4().hex
    session_name = (body.name if body else None) or name
    metadata: dict = {}
    if body and body.cwd:
        metadata["cwd"] = body.cwd
        # The session's original working directory, set once here and never
        # overwritten again (see `update_session_config`) — the directory-
        # sandboxing gate (`bigtiny/agent/sandbox.py`) always allows this
        # directory even after an agent-mode session's `cwd` is later
        # repointed elsewhere via "Set as working directory".
        metadata["chat_dir"] = body.cwd
    metadata["mode"] = (body.mode if body and body.mode else None) or "chat"
    await db.execute(
        "INSERT INTO sessions (id, name, metadata) VALUES (:id, :name, :meta)",
        {"id": session_id, "name": session_name, "meta": json.dumps(metadata)},
    )
    return {"session_id": session_id}


@router.post("/{session_id}/fork")
async def fork_session(session_id: str, body: ForkRequest, request: Request):
    """Copy a session and its messages (optionally truncated) into a new one."""
    db = request.app.state.db
    src = await db.fetch_one(
        "SELECT * FROM sessions WHERE id = :id", {"id": session_id}
    )
    if not src:
        raise HTTPException(404, "Session not found")

    rows = await db.fetch_all(
        "SELECT * FROM messages WHERE session_id = :sid "
        "ORDER BY created_at ASC, rowid ASC",
        {"sid": session_id},
    )
    if body.at_message_id:
        cut = next(
            (i for i, r in enumerate(rows) if r["id"] == body.at_message_id), None
        )
        if cut is None:
            raise HTTPException(404, "at_message_id not found in session")
        rows = rows[: cut + 1]

    metadata = json.loads(src["metadata"]) if src.get("metadata") else {}
    metadata["forked_from"] = session_id

    new_id = uuid4().hex
    await db.execute(
        "INSERT INTO sessions (id, name, metadata) VALUES (:id, :name, :meta)",
        {
            "id": new_id,
            "name": f"{src['name']} (branch)" if src.get("name") else None,
            "meta": json.dumps(metadata),
        },
    )
    for row in rows:
        await db.execute(
            "INSERT INTO messages "
            "(id, session_id, role, content, tool_calls, tool_call_id, "
            " content_format, token_count, created_at) "
            "VALUES (:id, :sid, :role, :content, :tc, :tcid, :fmt, :tokens, :ts)",
            {
                "id": uuid4().hex,
                "sid": new_id,
                "role": row["role"],
                "content": row["content"],
                "tc": row["tool_calls"],
                "tcid": row.get("tool_call_id"),
                "fmt": row.get("content_format") or "text",
                "tokens": row.get("token_count") or 0,
                "ts": row["created_at"],
            },
        )
    return {"session_id": new_id, "copied_messages": len(rows)}


@router.patch("/{session_id}/config")
async def update_session_config(
    session_id: str, body: SessionConfigRequest, request: Request
):
    """Set per-session provider/model/persona; the agent reads these from
    session metadata on each run."""
    db = request.app.state.db
    session = await db.fetch_one(
        "SELECT * FROM sessions WHERE id = :id", {"id": session_id}
    )
    if not session:
        raise HTTPException(404, "Session not found")

    metadata = json.loads(session["metadata"]) if session.get("metadata") else {}
    for key in ("provider", "model", "persona_override"):
        value = getattr(body, key)
        if value is not None:
            if value == "":
                metadata.pop(key, None)  # empty string clears the override
            else:
                metadata[key] = value

    if body.mode is not None:
        metadata["mode"] = body.mode

    if body.cwd is not None:
        # Directory sandboxing (`bigtiny/agent/sandbox.py`) only ever widens
        # an *agentic-mode* session's allowed directories with a
        # user-repointed `cwd` ("Set as working directory") — chat mode has
        # no UI path to change its cwd at all, but this check is
        # defense-in-depth against a buggy/future client trying to repoint
        # one directly over the API. `chat_dir` is deliberately never
        # touched here — it's write-once at session creation (see
        # `create_session`).
        if metadata.get("mode") == "agentic":
            metadata["cwd"] = body.cwd

    await db.execute(
        "UPDATE sessions SET metadata = :meta, updated_at = CURRENT_TIMESTAMP "
        "WHERE id = :id",
        {"id": session_id, "meta": json.dumps(metadata)},
    )
    return {"status": "updated", "config": {
        k: metadata.get(k) for k in ("provider", "model", "persona_override")
    }}


@router.patch("/{session_id}")
async def rename_session(session_id: str, body: RenameSessionRequest, request: Request):
    db = request.app.state.db
    session = await db.fetch_one(
        "SELECT * FROM sessions WHERE id = :id", {"id": session_id}
    )
    if not session:
        raise HTTPException(404, "Session not found")
    await db.execute(
        "UPDATE sessions SET name = :name, updated_at = CURRENT_TIMESTAMP "
        "WHERE id = :id",
        {"id": session_id, "name": body.name},
    )
    return {"status": "updated"}


@router.delete("/{session_id}")
async def delete_session(session_id: str, request: Request):
    db = request.app.state.db
    await db.execute("DELETE FROM sessions WHERE id = :id", {"id": session_id})
    return {"status": "deleted"}
