from __future__ import annotations

import json
from typing import Any
from uuid import uuid4

from bigtiny.config import TokenManagementConfig
from bigtiny.storage import Database
from bigtiny.models.mcp_server import ToolDefinition
from bigtiny.agent.tokens import count_message_tokens, count_messages_tokens
from bigtiny.agent.compaction import (
    apply_content_mask,
    apply_tool_mask,
    emergency_trim,
    find_reserve_floor_rowid,
    render_memory_block,
)

BASE_PERSONA = (
    "You are a helpful, precise AI assistant. "
    "Respond concisely and accurately."
)

# How much of the model's context window a fully-assembled prompt may
# occupy before the synchronous emergency valve (Phase 5) kicks in.
# Deliberately higher than compaction_threshold (the background-compaction
# trigger) — this only fires when background compaction has genuinely
# fallen behind (e.g. a single turn produced a huge burst of tool output),
# not as the normal path.
EMERGENCY_TRIM_RATIO = 0.9


class ContextManager:
    def __init__(
        self,
        db: Database,
        config: TokenManagementConfig,
        reserve_exchanges: int = 3,
    ):
        self.db = db
        self.config = config
        self.reserve_exchanges = reserve_exchanges

    async def build_messages(
        self,
        session_id: str,
        new_message: str,
        active_tools: list[ToolDefinition],
        persona_override: str | None = None,
        images: list[dict[str, str]] | None = None,
        max_context_tokens_override: int | None = None,
    ) -> list[dict[str, Any]]:
        session = await self.db.fetch_one(
            "SELECT memory_slots, compacted_through_rowid FROM sessions WHERE id = :id",
            {"id": session_id},
        )
        compacted_through = (session or {}).get("compacted_through_rowid") or 0
        memory_slots = (
            json.loads(session["memory_slots"])
            if session and session.get("memory_slots")
            else None
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

        # Layer 4: anchor the very first user message verbatim, so the
        # original project goal survives compaction entirely — it is never
        # part of the compacted/masked span below, regardless of how far
        # `compacted_through_rowid` has advanced. Only done when that
        # message is plain text: a multimodal (blocks) first message can't
        # be flattened into a system string without destroying its image
        # content, so it's left in the live section below instead (rowid 1
        # is always > compacted_through on any session that hasn't already
        # folded it away, so nothing is lost either way).
        #
        # Fetched as its own cheap, indexed LIMIT-1 lookup rather than
        # pulled out of a full-history scan — this message's row never
        # changes once written, so there's no reason to re-fetch the whole
        # session's history just to find it. This is what lets the Layer 6
        # query below be bounded to the live tail instead of the full
        # session (the previous unbounded `SELECT ... ORDER BY rowid ASC`
        # with no `WHERE rowid > :through` meant every turn re-fetched and
        # re-decoded the entire message history, growing without bound over
        # a session's lifetime).
        first_user_row = await self.db.fetch_one(
            "SELECT rowid, content, content_format FROM messages "
            "WHERE session_id = :sid AND role = 'user' ORDER BY rowid ASC LIMIT 1",
            {"sid": session_id},
        )
        first_user_content = self._row_content(first_user_row) if first_user_row else None
        anchor_first_user = first_user_row is not None and isinstance(first_user_content, str)
        if anchor_first_user:
            messages.append({
                "role": "system",
                "content": f"[Original request]\n{first_user_content}",
            })

        # Layer 5: consolidated memory from prior Tier-2 compaction passes.
        memory_block = render_memory_block(memory_slots)
        if memory_block:
            messages.append({"role": "system", "content": memory_block})

        # Layer 6: everything not yet folded into memory, Tier-1 masked.
        # Bounded to `rowid > compacted_through` — mirrors the query
        # `compaction.py`'s `run_compaction` already uses, so the two are
        # now consistent instead of one being bounded and the other not.
        first_user_rowid = first_user_row["rowid"] if anchor_first_user else None
        live_rows = await self.db.fetch_all(
            "SELECT rowid, * FROM messages WHERE session_id = :sid "
            "AND role != 'system' AND rowid > :through ORDER BY rowid ASC",
            {"sid": session_id, "through": compacted_through},
        )
        if first_user_rowid is not None:
            live_rows = [r for r in live_rows if r["rowid"] != first_user_rowid]
        # token_count is computed and persisted once at insert time
        # (save_messages) — summing it here instead of re-running tiktoken
        # over freshly-decoded content is what keeps this per-turn check
        # cheap regardless of history size. Tier-1 masking only ever
        # shrinks a message's rendered content, so a stale (pre-mask)
        # count is a safe overestimate for the emergency-valve check below:
        # it can only make the valve trigger slightly more eagerly, never
        # let an over-budget prompt through.
        live_token_sum = sum((r.get("token_count") or 0) for r in live_rows)
        live_messages = [self._row_to_message(r) for r in live_rows]
        reserve_floor = find_reserve_floor_rowid(live_rows, self.reserve_exchanges)
        live_messages = apply_tool_mask(live_messages, reserve_floor, self.config)
        live_messages = apply_content_mask(live_messages, reserve_floor, self.config)
        live_messages = self._enforce_live_tail_budget(live_messages, reserve_floor)
        # Captured before extending with the (potentially large) live tail,
        # so the emergency-valve check below can tiktoken-count just the
        # small, fixed system layers instead of the whole assembled prompt.
        head = list(messages)
        messages.extend(live_messages)

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
        tail_new_message = messages[-1]

        max_context_tokens = max_context_tokens_override or self.config.max_context_tokens
        emergency_cap = max_context_tokens * EMERGENCY_TRIM_RATIO
        # Only the system layers (small, fixed) and the just-appended new
        # message (not yet persisted, so it has no token_count row to sum)
        # need live tiktoken counting; the potentially-large live tail uses
        # the persisted sum above instead.
        total_tokens = (
            live_token_sum + self._count_tokens(head) + self._count_tokens([tail_new_message])
        )
        if total_tokens > emergency_cap:
            target = max_context_tokens * self.config.compaction_target_ratio
            # Only the live/masked tail (Layer 6) is eligible for the
            # synchronous trim — the system layers, anchor, and memory
            # block are small and never dropped.
            trimmed_live = emergency_trim(live_messages, reserve_floor, target)
            messages = head + trimmed_live + [tail_new_message]

        return messages

    def _enforce_live_tail_budget(
        self, live_messages: list[dict[str, Any]], reserve_floor: int
    ) -> list[dict[str, Any]]:
        """Per-turn budget check for the live, uncompacted tail (Layer 6) —
        independent of the whole-prompt compaction trigger below, since a
        single turn's live tail can already be large before background
        compaction gets a chance to run. Tool compaction (Phase A) is tried
        first: cheap, deterministic, structure-preserving. Exchange-level
        dropping (Phase B, reusing `emergency_trim`) is the last resort."""
        budget = self.config.max_live_tail_tokens
        if self._count_tokens(live_messages) <= budget:
            return live_messages

        # Phase A: collapse every live-tail tool message down to a bare
        # elision marker (head=tail=0 makes apply_tool_mask's own
        # `len(content) > head + tail` gate mask anything non-empty).
        zero_tool_mask_cfg = self.config.model_copy(
            update={"tool_mask_head": 0, "tool_mask_tail": 0}
        )
        live_messages = apply_tool_mask(live_messages, reserve_floor, zero_tool_mask_cfg)
        if self._count_tokens(live_messages) <= budget:
            return live_messages

        # Phase B: drop whole eligible exchanges, oldest first, targeting
        # this per-turn budget instead of the global compaction_target_ratio.
        return emergency_trim(live_messages, reserve_floor, budget)

    @staticmethod
    def _row_content(row: dict[str, Any]) -> Any:
        content: Any = row.get("content") or ""
        if row.get("content_format") == "blocks" and content:
            content = json.loads(content)
        return content

    def _row_to_message(self, row: dict[str, Any]) -> dict[str, Any]:
        # Keeps the DB id (so save_messages can tell persisted history
        # apart from messages produced during this run) and rowid (so
        # apply_tool_mask/emergency_trim can key off it).
        msg: dict[str, Any] = {
            "id": row["id"],
            "rowid": row["rowid"],
            "role": row["role"],
            "content": self._row_content(row),
        }
        if row["tool_calls"]:
            msg["tool_calls"] = json.loads(row["tool_calls"])
        if row.get("tool_call_id"):
            msg["tool_call_id"] = row["tool_call_id"]
        return msg

    def _count_tokens(self, messages: list[dict[str, Any]]) -> int:
        return count_messages_tokens(messages)

    async def count_tokens(self, messages: list[dict[str, Any]]) -> int:
        return self._count_tokens(messages)

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
            token_count = count_message_tokens(msg)
            await self.db.execute(
                "INSERT INTO messages "
                "(id, session_id, role, content, tool_calls, tool_call_id, "
                " content_format, token_count) "
                "VALUES (:id, :sid, :role, :content, :tc, :tcid, :fmt, :tok)",
                {
                    "id": msg_id,
                    "sid": session_id,
                    "role": msg["role"],
                    "content": content,
                    "tc": json.dumps(tool_calls) if tool_calls else None,
                    "tcid": msg.get("tool_call_id"),
                    "fmt": content_format,
                    "tok": token_count,
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
        # Only `role`/`token_count` are ever read below — projecting just
        # those (instead of `SELECT *`, which pulls every row's full
        # content/tool_calls JSON into memory only to discard it) is what
        # keeps this cheap on a long session. No ORDER BY needed either:
        # the sums below are order-independent, and dropping it lets SQLite
        # skip the sort it otherwise has to do (no index exists on
        # `created_at` — see the perf-indexes migration's comment for why
        # `rowid` is used for ordering elsewhere instead).
        messages = await self.db.fetch_all(
            "SELECT role, token_count FROM messages WHERE session_id = :sid",
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
        # token_count is populated accurately at insert time (save_messages),
        # so this is a straight sum rather than re-deriving from raw DB rows
        # (whose tool_calls/content columns are already-serialized JSON
        # strings, not the decoded objects count_message_tokens expects).
        current_context = sum(m.get("token_count", 0) or 0 for m in messages)

        meta = {}
        if session and session.get("metadata"):
            meta = json.loads(session["metadata"])

        cost_tokens = tokens_sent + tokens_received
        estimated_cost = round(cost_tokens * 0.000003, 6) if cost_tokens > 0 else 0

        memory_slots = (
            json.loads(session["memory_slots"])
            if session and session.get("memory_slots")
            else None
        )

        return {
            "session_id": session_id,
            "message_count": len(messages),
            "tokens_sent": tokens_sent,
            "tokens_received": tokens_received,
            "current_context_tokens": current_context,
            "estimated_cost_usd": estimated_cost,
            "provider_history": meta.get("usage", []),
            "compacted_through_rowid": (session or {}).get("compacted_through_rowid") or 0,
            "memory_slots": memory_slots,
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
