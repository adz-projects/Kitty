from __future__ import annotations

import json
import logging
import re
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Any

from bigtiny.agent.tokens import count_message_tokens, count_messages_tokens
from bigtiny.config import SummarizerConfig, TokenManagementConfig
from bigtiny.providers.summarizer_client import SummarizerClient, SummarizerError
from bigtiny.storage import Database

logger = logging.getLogger(__name__)


MEMORY_SLOT_KEYS = ("new_constraints", "new_decisions", "new_completions")

MEMORY_SLOTS_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "new_constraints": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Non-negotiable rules, exact paths, or technical "
            "bounds introduced in this chunk that were not already known.",
        },
        "new_decisions": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Verified facts, architecture choices, or agreed "
            "specs established in this chunk.",
        },
        "new_completions": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Tasks completed or code changes implemented in "
            "this chunk.",
        },
        "current_state": {
            "type": "string",
            "description": "The immediate focus area or next step, as of "
            "the end of this chunk.",
        },
    },
    "required": ["new_constraints", "new_decisions", "new_completions", "current_state"],
}

_SUMMARIZER_INSTRUCTIONS = (
    "You are compacting an AI coding assistant's conversation history. You "
    "are given EXISTING PROJECT MEMORY (already known) and a NEW CHUNK of "
    "conversation. Extract ONLY items from the new chunk that are not "
    "already covered by existing memory — do not repeat existing items, do "
    "not restate the whole history. Set current_state to the immediate "
    "focus/next-step as of the end of the new chunk. Respond with JSON "
    "matching the given schema only."
)


def render_memory_block(slots: dict[str, Any] | None) -> str | None:
    """Renders persisted memory slots as the `[CONSOLIDATED PROJECT MEMORY]`
    system block used in prompt assembly. Returns None when there is
    nothing to show yet (new/short sessions), so callers can omit the
    block entirely rather than emit an empty shell.
    """
    if not slots:
        return None
    lines = ["[CONSOLIDATED PROJECT MEMORY]"]
    labels = {
        "new_constraints": "User Constraints",
        "new_decisions": "Key Decisions",
        "new_completions": "Completed Actions",
    }
    has_content = False
    for key, label in labels.items():
        items = slots.get(key) or []
        if items:
            has_content = True
            lines.append(f"- {label}:")
            for item in items:
                lines.append(f"  - {item}")
    current_state = slots.get("current_state")
    if current_state:
        has_content = True
        lines.append(f"- Current State: {current_state}")
    if not has_content:
        return None
    return "\n".join(lines)


def merge_memory_slots(
    existing: dict[str, Any] | None, new: dict[str, Any]
) -> dict[str, Any]:
    """Append-only merge: list slots only ever grow (deduped), never get
    rewritten wholesale — a bad summarizer pass can add noise but can never
    erase or corrupt memory a prior pass already committed. `current_state`
    is the sole field that gets overwritten, since it is explicitly a
    snapshot of "as of now", not a history.
    """
    merged: dict[str, Any] = {k: list(existing.get(k, [])) if existing else [] for k in MEMORY_SLOT_KEYS}
    for key in MEMORY_SLOT_KEYS:
        seen = {item.strip().lower() for item in merged[key]}
        for item in new.get(key) or []:
            item = str(item).strip()
            if item and item.lower() not in seen:
                merged[key].append(item)
                seen.add(item.lower())
    merged["current_state"] = str(new.get("current_state") or (existing or {}).get("current_state") or "")
    return merged


def consolidate_slot_if_needed(slots: dict[str, Any], max_items: int) -> dict[str, Any]:
    """Bounded, isolated shrink: when a single list grows past max_items,
    keep only the most recent max_items entries. Deliberately dumb (no LLM
    call) — a real consolidation pass would itself need to summarize
    without dropping load-bearing constraints, which is exactly the drift
    risk this whole design tries to avoid. Recency is an acceptable proxy:
    older, still-relevant constraints tend to get restated or superseded
    over a long session anyway.
    """
    for key in MEMORY_SLOT_KEYS:
        items = slots.get(key) or []
        if len(items) > max_items:
            slots[key] = items[-max_items:]
    return slots


def apply_tool_mask(
    messages: list[dict[str, Any]],
    reserve_floor_rowid: int,
    cfg: TokenManagementConfig,
) -> list[dict[str, Any]]:
    """Tier 1: deterministic, zero-LLM-cost tool-output elision. Any
    `role="tool"` message whose `rowid` has aged past the reserve floor
    (i.e. it's no longer part of the live, most-recent exchanges) and whose
    content exceeds `tool_mask_head + tool_mask_tail` gets its middle
    replaced with an elision marker.

    Keeps both head AND tail (not just a head truncation) since the
    informative part of tool output often clusters at the tail — e.g. a
    traceback's actual exception line, or a command's final "N passed / M
    failed" summary line.

    Once a message crosses the reserve floor it is masked identically on
    every subsequent render — the same rowid always yields the same masked
    content — so this only ever changes a given message's rendered content
    exactly once (the turn it first ages out of the live window), not on
    every turn, which is what keeps the KV-cache-relevant portion of the
    prompt from being rewritten repeatedly.
    """
    head, tail = cfg.tool_mask_head, cfg.tool_mask_tail
    out: list[dict[str, Any]] = []
    for msg in messages:
        if (
            msg.get("role") == "tool"
            and msg.get("rowid") is not None
            and msg["rowid"] < reserve_floor_rowid
            and isinstance(msg.get("content"), str)
            and len(msg["content"]) > head + tail
        ):
            content = msg["content"]
            elided = len(content) - head - tail
            masked = dict(msg)
            masked["content"] = (
                content[:head]
                + f"\n[...{elided} bytes elided; re-run the tool if you need the full output...]\n"
                + content[len(content) - tail :]
            )
            out.append(masked)
        else:
            out.append(msg)
    return out


_FENCE_RE = re.compile(r"```[^\n]*\n.*?\n```", re.DOTALL)


def _mask_code_block(fence_block: str, head_lines: int, tail_lines: int) -> str:
    """Masks one ```...``` fenced block (including its fences) down to its
    head_lines + tail_lines, keeping the opening fence line (fence + optional
    language tag) and the closing fence untouched. Assumes `fence_block`
    already matched `_FENCE_RE`, so it starts with ``` and ends with ```."""
    lines = fence_block.split("\n")
    opening, closing = lines[0], lines[-1]
    body = lines[1:-1]
    if len(body) <= head_lines + tail_lines:
        return fence_block
    elided = len(body) - head_lines - tail_lines
    kept = (
        body[:head_lines]
        + [f"[...{elided} lines elided...]"]
        + (body[len(body) - tail_lines :] if tail_lines else [])
    )
    return "\n".join([opening, *kept, closing])


def apply_content_mask(
    messages: list[dict[str, Any]],
    reserve_floor_rowid: int,
    cfg: TokenManagementConfig,
) -> list[dict[str, Any]]:
    """Tier 1: deterministic, zero-LLM-cost masking of large fenced code
    blocks inside `user`/`assistant` message content. Same eligibility rule
    and KV-cache-stability contract as `apply_tool_mask` (masks only
    messages whose rowid has aged past the reserve floor, and does so
    identically on every subsequent render), applied to a different
    surface: users/assistants pasting large code blocks, not tool output.

    Prose outside fences and single-backtick inline code are left untouched
    — only content between matched ```-fences is a masking candidate.
    """
    head, tail = cfg.message_mask_head_lines, cfg.message_mask_tail_lines
    out: list[dict[str, Any]] = []
    for msg in messages:
        if (
            msg.get("role") not in ("user", "assistant")
            or msg.get("rowid") is None
            or msg["rowid"] >= reserve_floor_rowid
            or not isinstance(msg.get("content"), str)
        ):
            out.append(msg)
            continue

        content = msg["content"]
        if "```" not in content:
            out.append(msg)
            continue

        masked_content = _FENCE_RE.sub(
            lambda m: _mask_code_block(m.group(0), head, tail), content
        )
        if masked_content == content:
            out.append(msg)
        else:
            masked = dict(msg)
            masked["content"] = masked_content
            out.append(masked)
    return out


def group_into_exchanges(rows: list[dict[str, Any]]) -> list[list[dict[str, Any]]]:
    """Groups rows into exchanges: each exchange starts at a `role="user"`
    message and runs up to (but not including) the next one. This is a safe
    grouping unit for both the reserve window and compaction spans because
    an assistant-with-tool_calls message and its paired `tool` replies are
    only ever produced within a single `Agent.run` — i.e. between one user
    message and the next — so grouping this way can never split a
    tool_call/tool-result pair across a boundary.
    """
    exchanges: list[list[dict[str, Any]]] = []
    current: list[dict[str, Any]] = []
    for row in rows:
        if row.get("role") == "user" and current:
            exchanges.append(current)
            current = []
        current.append(row)
    if current:
        exchanges.append(current)
    return exchanges


def find_reserve_floor_rowid(rows: list[dict[str, Any]], reserve_exchanges: int) -> int:
    """Returns the rowid of the first message that must stay in the live,
    uncompacted window — everything at or after this rowid is one of the
    last `reserve_exchanges` complete exchanges. Returns 0 (nothing
    reserved, i.e. no eligible masking/compaction target exists yet) when
    there are fewer than `reserve_exchanges` complete exchanges in the
    given rows.
    """
    exchanges = group_into_exchanges(rows)
    if len(exchanges) <= reserve_exchanges:
        return rows[0]["rowid"] if rows else 0
    reserved = exchanges[-reserve_exchanges:]
    return reserved[0][0]["rowid"]


@dataclass
class CompactionResult:
    messages_compacted: int
    tokens_before: int
    tokens_after: int


_STALE_LOCK_MULTIPLIER = 2


async def _try_acquire_lock(db: Database, session_id: str, timeout_s: float) -> bool:
    stale_cutoff = (
        datetime.now(timezone.utc) - timedelta(seconds=timeout_s * _STALE_LOCK_MULTIPLIER)
    ).strftime("%Y-%m-%d %H:%M:%S")
    # Compare-and-swap, not read-then-write: two `run_compaction` calls for
    # the same session could otherwise both observe 'idle' before either
    # writes 'running' (SQLite here is autocommit — see storage.py's
    # isolation_level=None comment — so there is no implicit transaction
    # isolating a read-then-write). The WHERE clause makes acquisition
    # atomic; `rowcount` tells the caller whether *this* call won it. A
    # `running` lock older than 2x the summarizer timeout is treated as
    # abandoned (e.g. the daemon crashed mid-pass) and reclaimed rather
    # than wedging that session's compaction forever.
    cursor = await db.execute(
        "UPDATE sessions SET compaction_state = 'running', "
        "compaction_started_at = CURRENT_TIMESTAMP "
        "WHERE id = :id AND ("
        "  compaction_state != 'running' OR "
        "  compaction_started_at IS NULL OR "
        "  compaction_started_at < :stale_cutoff"
        ")",
        {"id": session_id, "stale_cutoff": stale_cutoff},
    )
    return cursor.rowcount > 0


async def _release_lock(db: Database, session_id: str) -> None:
    await db.execute(
        "UPDATE sessions SET compaction_state = 'idle' WHERE id = :id",
        {"id": session_id},
    )


async def run_compaction(
    session_id: str,
    db: Database,
    summarizer: SummarizerClient,
    token_cfg: TokenManagementConfig,
    summarizer_cfg: SummarizerConfig,
    context_length: int,
) -> CompactionResult | None:
    """The full Tier 1 + Tier 2 compaction pass for one session. Safe to
    call speculatively after every turn (the daemon does, from the agent
    run's `finally` block) — it no-ops quickly whenever there's nothing
    eligible or the session is below threshold, and never raises: any
    failure is caught, logged, and leaves the session's persisted state
    exactly as it was so the next turn's attempt starts fresh.
    """
    if not summarizer_cfg.enabled:
        return None

    if not await _try_acquire_lock(db, session_id, summarizer_cfg.timeout_s):
        return None

    try:
        session = await db.fetch_one(
            "SELECT * FROM sessions WHERE id = :id", {"id": session_id}
        )
        if not session:
            return None

        compacted_through = session.get("compacted_through_rowid") or 0
        existing_slots = (
            json.loads(session["memory_slots"]) if session.get("memory_slots") else None
        )

        rows = await db.fetch_all(
            "SELECT rowid, * FROM messages WHERE session_id = :sid AND rowid > :through "
            "ORDER BY rowid ASC",
            {"sid": session_id, "through": compacted_through},
        )
        rows = [r for r in rows if r.get("role") != "system"]
        if not rows:
            return None

        reserve_floor = find_reserve_floor_rowid(rows, summarizer_cfg.reserve_exchanges)
        candidate_rows = [r for r in rows if r["rowid"] < reserve_floor]
        if not candidate_rows:
            return None

        # token_count is persisted per row at insert time (save_messages);
        # summing that column directly avoids re-decoding every row's JSON
        # content/tool_calls and re-running tiktoken over it just to check
        # a threshold — the same optimization applied to the per-turn
        # emergency-valve check in context_manager.py's build_messages.
        total_tokens = sum((r.get("token_count") or 0) for r in rows)
        high_water = max(
            token_cfg.min_compaction_tokens, context_length * token_cfg.compaction_threshold
        )
        if total_tokens <= high_water:
            return None

        low_water = context_length * token_cfg.compaction_target_ratio

        # Fold the oldest eligible exchanges, one at a time, until either
        # the whole candidate span is consumed or projected total tokens
        # (everything else minus what we're folding) drops to the target —
        # a single summarizer call over exactly that span, not a loop of
        # calls, since a sub-1B model's latency-per-call makes repeated
        # calls the wrong trade here.
        candidate_exchanges = group_into_exchanges(candidate_rows)
        to_fold: list[dict[str, Any]] = []
        remaining_tokens = total_tokens
        for exchange in candidate_exchanges:
            to_fold.extend(exchange)
            remaining_tokens -= sum((r.get("token_count") or 0) for r in exchange)
            if remaining_tokens <= low_water:
                break

        if not to_fold:
            return None

        masked_for_summary = apply_tool_mask(
            _deserialize_rows(to_fold), reserve_floor, token_cfg
        )

        summary_messages = _build_summarizer_prompt(existing_slots, masked_for_summary)
        try:
            result = await summarizer.structured_chat(summary_messages, MEMORY_SLOTS_SCHEMA)
        except SummarizerError as e:
            logger.warning("compaction: summarizer call failed for %s: %s", session_id, e)
            return None

        merged = merge_memory_slots(existing_slots, result)
        merged = consolidate_slot_if_needed(merged, summarizer_cfg.max_slot_items)
        new_watermark = to_fold[-1]["rowid"]

        await db.execute(
            "UPDATE sessions SET memory_slots = :slots, "
            "compacted_through_rowid = :through, compaction_state = 'idle' "
            "WHERE id = :id",
            {"slots": json.dumps(merged), "through": new_watermark, "id": session_id},
        )

        tokens_after = total_tokens - sum((r.get("token_count") or 0) for r in to_fold)
        return CompactionResult(
            messages_compacted=len(to_fold),
            tokens_before=total_tokens,
            tokens_after=tokens_after,
        )
    except Exception:
        logger.exception("compaction: unexpected failure for session %s", session_id)
        return None
    finally:
        # Only meaningful if the try block returned/raised before its own
        # explicit 'idle' UPDATE (the success path already sets it) — a
        # second identical UPDATE here is harmless.
        await _release_lock(db, session_id)


def _deserialize_rows(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    out = []
    for row in rows:
        content: Any = row.get("content") or ""
        if row.get("content_format") == "blocks" and content:
            content = json.loads(content)
        msg: dict[str, Any] = {
            "rowid": row.get("rowid"),
            "role": row["role"],
            "content": content,
        }
        if row.get("tool_calls"):
            msg["tool_calls"] = json.loads(row["tool_calls"])
        if row.get("tool_call_id"):
            msg["tool_call_id"] = row["tool_call_id"]
        out.append(msg)
    return out


def emergency_trim(
    messages: list[dict[str, Any]],
    reserve_floor_rowid: int,
    target_tokens: int,
) -> list[dict[str, Any]]:
    """Phase 5 synchronous fallback: if background compaction has fallen
    behind badly enough that the assembled prompt still exceeds the hard
    cap even after Tier 1 masking, drop whole exchanges — oldest first,
    from the already-eligible-for-compaction region only, never touching
    the reserved live tail — until under `target_tokens`. Deterministic,
    no LLM call, and pairing-safe by construction (exchanges are the same
    tool-call-pairing-safe unit used everywhere else in this module).

    This only fires when the prompt is still too big *right now*; it is
    not a substitute for background compaction and does not touch
    persisted state — it only affects what gets sent for this one turn.
    """
    def _is_eligible(m: dict[str, Any]) -> bool:
        return m.get("rowid") is not None and m["rowid"] < reserve_floor_rowid

    eligible = [m for m in messages if _is_eligible(m)]
    reserved = [m for m in messages if not _is_eligible(m)]
    if not eligible:
        return messages

    exchanges = group_into_exchanges(eligible)
    total = count_messages_tokens(messages)
    dropped_count = 0
    kept_exchanges: list[list[dict[str, Any]]] = list(exchanges)

    while total > target_tokens and kept_exchanges:
        victim = kept_exchanges.pop(0)
        total -= count_messages_tokens(victim)
        dropped_count += len(victim)

    result: list[dict[str, Any]] = []
    if dropped_count:
        result.append({
            "role": "system",
            "content": f"[{dropped_count} earlier tool interactions elided to fit context]",
        })
    for exchange in kept_exchanges:
        result.extend(exchange)
    result.extend(reserved)
    return result


def _build_summarizer_prompt(
    existing_slots: dict[str, Any] | None, chunk: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    existing_block = json.dumps(existing_slots) if existing_slots else "(none yet)"
    chunk_lines = []
    for msg in chunk:
        role = msg["role"]
        content = msg.get("content", "")
        if isinstance(content, list):
            content = json.dumps(content)
        if msg.get("tool_calls"):
            content = f"{content} [tool_calls: {json.dumps(msg['tool_calls'])}]"
        chunk_lines.append(f"{role}: {content}")

    return [
        {"role": "system", "content": _SUMMARIZER_INSTRUCTIONS},
        {
            "role": "user",
            "content": (
                f"EXISTING PROJECT MEMORY:\n{existing_block}\n\n"
                f"NEW CHUNK:\n" + "\n".join(chunk_lines)
            ),
        },
    ]
