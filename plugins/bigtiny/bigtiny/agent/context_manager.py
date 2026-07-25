from __future__ import annotations

import json
from typing import Any
from uuid import uuid4

from bigtiny.config import TokenManagementConfig
from bigtiny.storage import Database
from bigtiny.models.mcp_server import ToolDefinition

BASE_PERSONA = (
    "You are a helpful, precise AI assistant. "
    "Respond concisely and accurately."
)


class ContextManager:
    def __init__(self, db: Database, config: TokenManagementConfig):
        self.db = db
        self.config = config

    async def build_messages(
        self,
        session_id: str,
        new_message: str,
        active_tools: list[ToolDefinition],
        persona_override: str | None = None,
        images: list[dict[str, str]] | None = None,
        max_context_tokens_override: int | None = None,
    ) -> list[dict[str, Any]]:
        # rowid tiebreaker: created_at has 1-second resolution, so messages
        # written in the same second would otherwise come back in random order.
        rows = await self.db.fetch_all(
            "SELECT * FROM messages WHERE session_id = :sid "
            "ORDER BY created_at ASC, rowid ASC",
            {"sid": session_id},
        )

        messages: list[dict[str, Any]] = []

        # Layer 1: Base persona
        messages.append({"role": "system", "content": BASE_PERSONA})

        # Layer 2: Session override
        if persona_override:
            messages.append({"role": "system", "content": persona_override})

        # Layer 3: Dynamic tool hints
        if active_tools:
            tool_hints_lines = ["You have access to the following MCP tools:"]
            for t in active_tools:
                params = list(t.input_schema.get("properties", {}).keys()) if t.input_schema else []
                sig = f"{t.name}({', '.join(params)})" if params else t.name
                desc = (t.description or "")[:120]
                tool_hints_lines.append(f"  - {sig}: {desc}")
            tool_hints_lines.append(
                "\nUse these tools when appropriate to fulfill the user's request."
            )
            messages.append({"role": "system", "content": "\n".join(tool_hints_lines)})

        for row in rows:
            if row["role"] == "system":
                continue
            # Keep the DB id so save_messages can tell persisted history
            # apart from messages produced during this run.
            content: Any = row["content"] or ""
            if row.get("content_format") == "blocks" and content:
                content = json.loads(content)
            msg: dict[str, Any] = {
                "id": row["id"],
                "role": row["role"],
                "content": content,
            }
            if row["tool_calls"]:
                msg["tool_calls"] = json.loads(row["tool_calls"])
            if row.get("tool_call_id"):
                msg["tool_call_id"] = row["tool_call_id"]
            messages.append(msg)

        if images:
            blocks: list[dict[str, Any]] = [{"type": "text", "text": new_message}]
            for img in images:
                blocks.append({
                    "type": "image",
                    "data": img.get("data", ""),
                    "mime_type": img.get("mime_type", "image/png"),
                })
            messages.append({"role": "user", "content": blocks})
        else:
            messages.append({"role": "user", "content": new_message})

        total_tokens = self._count_tokens(messages)
        max_context_tokens = max_context_tokens_override or self.config.max_context_tokens
        threshold = max_context_tokens * self.config.compaction_threshold
        if total_tokens > threshold:
            messages = await self._compact(session_id, messages)

        return messages

    def _count_tokens(self, messages: list[dict[str, Any]]) -> int:
        total = 0
        for msg in messages:
            content = str(msg.get("content", ""))
            total += len(content) // 4
        return total

    async def count_tokens(self, messages: list[dict[str, Any]]) -> int:
        return self._count_tokens(messages)

    async def _compact(
        self,
        session_id: str,
        messages: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        system_msgs = [m for m in messages if m["role"] == "system"]
        non_system = [m for m in messages if m["role"] != "system"]

        kept = list(system_msgs)
        keep_count = min(len(non_system), 4)
        keep_messages = non_system[-keep_count:] if keep_count > 0 else []
        to_summarize = non_system[:-keep_count] if len(non_system) > keep_count else []

        if to_summarize:
            total_text = " ".join(
                str(m.get("content", "")) for m in to_summarize if m.get("content")
            )
            summary = (total_text[:300] + "...") if len(total_text) > 300 else total_text
            kept.append({
                "role": "system",
                "content": f"[Previous conversation summarized: {summary}]",
            })

        kept.extend(keep_messages)
        return kept

    async def save_messages(
        self,
        session_id: str,
        messages: list[dict[str, Any]],
    ) -> None:
        for msg in messages:
            if msg.get("id") or msg["role"] == "system":
                continue
            msg_id = uuid4().hex
            content = msg.get("content", "")
            content_format = "text"
            if isinstance(content, list):
                content = json.dumps(content)
                content_format = "blocks"
            tool_calls = msg.get("tool_calls")
            await self.db.execute(
                "INSERT INTO messages "
                "(id, session_id, role, content, tool_calls, tool_call_id, "
                " content_format, token_count) "
                "VALUES (:id, :sid, :role, :content, :tc, :tcid, :fmt, 0)",
                {
                    "id": msg_id,
                    "sid": session_id,
                    "role": msg["role"],
                    "content": content,
                    "tc": json.dumps(tool_calls) if tool_calls else None,
                    "tcid": msg.get("tool_call_id"),
                    "fmt": content_format,
                },
            )
        await self.db.execute(
            "UPDATE sessions SET updated_at = CURRENT_TIMESTAMP WHERE id = :id",
            {"id": session_id},
        )


class SessionStats:
    def __init__(self, db: Database):
        self.db = db

    async def get_stats(self, session_id: str) -> dict[str, object]:
        messages = await self.db.fetch_all(
            "SELECT * FROM messages WHERE session_id = :sid ORDER BY created_at ASC",
            {"sid": session_id},
        )
        session = await self.db.fetch_one(
            "SELECT * FROM sessions WHERE id = :id", {"id": session_id}
        )

        tokens_sent = sum(
            m.get("token_count", 0)
            for m in messages
            if m["role"] in ("user", "system")
        )
        tokens_received = sum(
            m.get("token_count", 0) for m in messages if m["role"] == "assistant"
        )
        current_context = sum(
            len(str(m.get("content", ""))) // 4 for m in messages
        )

        meta = {}
        if session and session.get("metadata"):
            meta = json.loads(session["metadata"])

        cost_tokens = tokens_sent + tokens_received
        estimated_cost = round(cost_tokens * 0.000003, 6) if cost_tokens > 0 else 0

        return {
            "session_id": session_id,
            "message_count": len(messages),
            "tokens_sent": tokens_sent,
            "tokens_received": tokens_received,
            "current_context_tokens": current_context,
            "estimated_cost_usd": estimated_cost,
            "provider_history": meta.get("usage", []),
        }

    async def record_usage(
        self,
        session_id: str,
        prompt_tokens: int,
        completion_tokens: int,
        provider: str,
        model: str,
    ) -> None:
        session = await self.db.fetch_one(
            "SELECT * FROM sessions WHERE id = :id", {"id": session_id}
        )
        if not session:
            return

        meta = json.loads(session["metadata"]) if session.get("metadata") else {}
        usage = meta.get("usage", [])
        usage.append({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "provider": provider,
            "model": model,
        })
        if len(usage) > 100:
            usage = usage[-100:]
        meta["usage"] = usage

        await self.db.execute(
            "UPDATE sessions SET metadata = :meta WHERE id = :id",
            {"id": session_id, "meta": json.dumps(meta)},
        )
