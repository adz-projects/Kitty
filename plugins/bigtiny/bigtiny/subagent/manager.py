from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from datetime import datetime
from typing import Awaitable, Callable
from uuid import uuid4

from bigtiny.agent.loop import Agent
from bigtiny.server.events import SSEEvent
from bigtiny.storage import Database

logger = logging.getLogger(__name__)


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


class SubagentManager:
    def __init__(self, agent: Agent, db: Database):
        self.agent = agent
        self.db = db
        self._subagents: dict[str, Subagent] = {}

    async def spawn(
        self,
        parent_session_id: str,
        prompt: str,
        provider_override: str | None = None,
        event_callback: Callable[[SSEEvent], Awaitable[None]] | None = None,
    ) -> str:
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
        events: list[SSEEvent] = []

        async def cb(event: SSEEvent):
            events.append(event)
            if event_callback:
                await event_callback(event)

        try:
            await self.agent.run(
                session_id=subagent.session_id,
                user_message=subagent.prompt,
                event_callback=cb,
                provider_override=provider_override,
            )

            last_content = None
            for e in reversed(events):
                if e.type == "llm_delta" and e.content:
                    last_content = e.content
                    break

            subagent.status = "completed"
            subagent.result = last_content or ""
            subagent.completed_at = datetime.utcnow()
        except Exception as e:
            logger.exception("Subagent %s failed", subagent.id)
            subagent.status = "failed"
            subagent.error = str(e)
            subagent.completed_at = datetime.utcnow()

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

        start = datetime.utcnow()
        while subagent.status == "running":
            elapsed = (datetime.utcnow() - start).total_seconds()
            if elapsed >= timeout:
                subagent.status = "failed"
                subagent.error = "Timed out"
                return subagent
            await asyncio.sleep(0.5)

        return subagent
