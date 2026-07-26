from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Awaitable, Callable
from uuid import uuid4

from bigtiny.agent.loop import Agent
from bigtiny.server.events import SSEEvent
from bigtiny.storage import Database

logger = logging.getLogger(__name__)

# `_subagents` is otherwise unbounded — every subagent ever spawned over
# the daemon's lifetime would stay in memory forever. Swept on each
# `spawn()` call (amortized, no dedicated background task) rather than
# evicted the instant a run completes, so `get_subagent`/`list_subagents`
# still work for a reasonable window after a run finishes.
SUBAGENT_RETENTION = timedelta(hours=1)


@dataclass
class Subagent:
    id: str
    session_id: str
    parent_session_id: str
    prompt: str
    status: str = "running"
    result: str | None = None
    error: str | None = None
    created_at: datetime = field(default_factory=datetime.utcnow)
    completed_at: datetime | None = None
    # Set once `_run_subagent` reaches a terminal status — lets
    # `wait_for_completion` await instead of polling `status` in a sleep
    # loop (`asyncio.Event()` can't be a dataclass field default directly
    # since it needs the running loop; constructed in `__post_init__`).
    done_event: asyncio.Event = field(init=False, repr=False)

    def __post_init__(self) -> None:
        self.done_event = asyncio.Event()


class SubagentManager:
    def __init__(self, agent: Agent, db: Database):
        self.agent = agent
        self.db = db
        self._subagents: dict[str, Subagent] = {}

    def _sweep_completed(self) -> None:
        cutoff = datetime.utcnow() - SUBAGENT_RETENTION
        stale_ids = [
            sid
            for sid, s in self._subagents.items()
            if s.completed_at is not None and s.completed_at < cutoff
        ]
        for sid in stale_ids:
            self._subagents.pop(sid, None)

    async def spawn(
        self,
        parent_session_id: str,
        prompt: str,
        provider_override: str | None = None,
        event_callback: Callable[[SSEEvent], Awaitable[None]] | None = None,
    ) -> str:
        self._sweep_completed()
        subagent_id = f"sub_{uuid4().hex[:12]}"
        session_id = uuid4().hex

        await self.db.execute(
            "INSERT INTO sessions (id, name, metadata) VALUES (:id, :name, :meta)",
            {
                "id": session_id,
                "name": f"subagent_{subagent_id}",
                "meta": json.dumps({"parent_session": parent_session_id}),
            },
        )

        subagent = Subagent(
            id=subagent_id,
            session_id=session_id,
            parent_session_id=parent_session_id,
            prompt=prompt,
        )
        self._subagents[subagent_id] = subagent

        asyncio.create_task(
            self._run_subagent(subagent, event_callback, provider_override)
        )

        return subagent_id

    async def _run_subagent(
        self,
        subagent: Subagent,
        event_callback: Callable[[SSEEvent], Awaitable[None]] | None,
        provider_override: str | None = None,
    ) -> None:
        # Only `llm_delta` chunks are ever read out of the run's events (to
        # build the final result text) — buffering every SSEEvent of the
        # entire run just to scan it afterward held the whole event stream
        # in memory for no reason. Accumulating just the content chunks
        # also fixes a pre-existing bug: the old scan took only the *last*
        # `llm_delta` event's own (incremental) `.content` as the result,
        # not the full joined response — for any streamed reply longer than
        # one chunk, `subagent.result` was silently just its final
        # fragment.
        content_chunks: list[str] = []

        async def cb(event: SSEEvent):
            if event.type == "llm_delta" and event.content:
                content_chunks.append(event.content)
            if event_callback:
                await event_callback(event)

        try:
            await self.agent.run(
                session_id=subagent.session_id,
                user_message=subagent.prompt,
                event_callback=cb,
                provider_override=provider_override,
            )

            subagent.status = "completed"
            subagent.result = "".join(content_chunks)
            subagent.completed_at = datetime.utcnow()
        except Exception as e:
            logger.exception("Subagent %s failed", subagent.id)
            subagent.status = "failed"
            subagent.error = str(e)
            subagent.completed_at = datetime.utcnow()
        finally:
            subagent.done_event.set()

    def get_subagent(self, subagent_id: str) -> Subagent | None:
        return self._subagents.get(subagent_id)

    def list_subagents(self, parent_session_id: str) -> list[Subagent]:
        return [
            s
            for s in self._subagents.values()
            if s.parent_session_id == parent_session_id
        ]

    async def wait_for_completion(
        self,
        subagent_id: str,
        timeout: float = 300,
    ) -> Subagent | None:
        subagent = self._subagents.get(subagent_id)
        if not subagent:
            return None

        if subagent.status == "running":
            try:
                await asyncio.wait_for(subagent.done_event.wait(), timeout=timeout)
            except asyncio.TimeoutError:
                subagent.status = "failed"
                subagent.error = "Timed out"

        return subagent
