"""Unit tests for the conversation-compaction subsystem
(bigtiny/agent/compaction.py). Run: pytest tests/test_compaction.py -v
"""

from __future__ import annotations

import asyncio
import json
from datetime import datetime, timedelta, timezone
from typing import Any
from uuid import uuid4

import pytest

from bigtiny.agent.compaction import (
    CompactionResult,
    apply_tool_mask,
    consolidate_slot_if_needed,
    emergency_trim,
    find_reserve_floor_rowid,
    group_into_exchanges,
    merge_memory_slots,
    render_memory_block,
    run_compaction,
)
from bigtiny.config import SummarizerConfig, TokenManagementConfig
from bigtiny.providers.summarizer_client import SummarizerError
from bigtiny.storage import Database


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def db():
    database = Database(":memory:")

    async def setup():
        await database.connect()
        return database

    instance = asyncio.run(setup())
    yield instance
    asyncio.run(instance.close())


@pytest.fixture
def token_cfg():
    return TokenManagementConfig(
        max_context_tokens=1000,
        compaction_threshold=0.8,
        compaction_target_ratio=0.5,
    )


@pytest.fixture
def summarizer_cfg():
    return SummarizerConfig(reserve_exchanges=1, timeout_s=5.0)


@pytest.fixture
def session_id():
    return uuid4().hex


async def _make_session(db: Database, session_id: str) -> None:
    await db.execute(
        "INSERT INTO sessions (id, name) VALUES (:id, :name)",
        {"id": session_id, "name": "test"},
    )


async def _insert_message(
    db: Database,
    session_id: str,
    role: str,
    content: str,
    tool_calls: list[dict[str, Any]] | None = None,
    tool_call_id: str | None = None,
) -> int:
    cursor = await db.execute(
        "INSERT INTO messages (id, session_id, role, content, tool_calls, tool_call_id, token_count) "
        "VALUES (:id, :sid, :role, :content, :tc, :tcid, :tok)",
        {
            "id": uuid4().hex,
            "sid": session_id,
            "role": role,
            "content": content,
            "tc": json.dumps(tool_calls) if tool_calls else None,
            "tcid": tool_call_id,
            "tok": len(content) // 4,
        },
    )
    return cursor.lastrowid


class FakeSummarizer:
    """Stands in for SummarizerClient — returns a fixed structured result,
    or raises SummarizerError if configured to fail."""

    def __init__(self, result: dict[str, Any] | None = None, fail: bool = False):
        self.result = result or {
            "new_constraints": ["use TypeScript strict mode"],
            "new_decisions": ["chose Zustand over Redux"],
            "new_completions": ["implemented the composer"],
            "current_state": "wiring up settings UI",
        }
        self.fail = fail
        self.calls = 0

    async def structured_chat(self, messages, json_schema):
        self.calls += 1
        if self.fail:
            raise SummarizerError("simulated failure")
        return self.result


# ---------------------------------------------------------------------------
# Pure helpers: grouping / reserve floor
# ---------------------------------------------------------------------------

def _rows(pairs: list[tuple[int, str]]) -> list[dict[str, Any]]:
    """rowid, role pairs -> row dicts, content is arbitrary."""
    return [{"rowid": rid, "role": role, "content": f"msg-{rid}"} for rid, role in pairs]


def test_group_into_exchanges_groups_by_user_boundary():
    rows = _rows([
        (1, "user"), (2, "assistant"), (3, "tool"), (4, "assistant"),
        (5, "user"), (6, "assistant"),
    ])
    exchanges = group_into_exchanges(rows)
    assert len(exchanges) == 2
    assert [r["rowid"] for r in exchanges[0]] == [1, 2, 3, 4]
    assert [r["rowid"] for r in exchanges[1]] == [5, 6]


def test_find_reserve_floor_rowid_reserves_last_n_exchanges():
    rows = _rows([
        (1, "user"), (2, "assistant"),
        (3, "user"), (4, "assistant"),
        (5, "user"), (6, "assistant"),
    ])
    floor = find_reserve_floor_rowid(rows, reserve_exchanges=2)
    # Last 2 exchanges start at rowid 3 -> everything before 3 is eligible.
    assert floor == 3


def test_find_reserve_floor_rowid_reserves_everything_when_too_few_exchanges():
    rows = _rows([(1, "user"), (2, "assistant")])
    floor = find_reserve_floor_rowid(rows, reserve_exchanges=3)
    assert floor == 1  # nothing eligible


# ---------------------------------------------------------------------------
# Tool-call pairing invariant
# ---------------------------------------------------------------------------

def test_apply_tool_mask_never_splits_tool_call_pairs():
    """Masking only ever rewrites `content`, never removes a message — so an
    assistant-with-tool_calls + its tool replies stay paired regardless of
    where the reserve floor lands."""
    long_output = "x" * 2000
    rows = [
        {"rowid": 1, "role": "user", "content": "do the thing"},
        {
            "rowid": 2, "role": "assistant", "content": "",
            "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "run"}}],
        },
        {"rowid": 3, "role": "tool", "content": long_output, "tool_call_id": "tc1"},
        {"rowid": 4, "role": "user", "content": "next"},
        {"rowid": 5, "role": "assistant", "content": "done"},
    ]
    cfg = TokenManagementConfig(tool_mask_head=10, tool_mask_tail=10)
    masked = apply_tool_mask(rows, reserve_floor_rowid=4, cfg=cfg)

    assert len(masked) == len(rows)
    ids_by_rowid = {m["rowid"]: m for m in masked}
    assert ids_by_rowid[2]["tool_calls"][0]["id"] == "tc1"
    assert ids_by_rowid[3]["tool_call_id"] == "tc1"
    assert "elided" in ids_by_rowid[3]["content"]
    # Reserved (rowid >= floor) messages are untouched.
    assert ids_by_rowid[4]["content"] == "next"


def test_apply_tool_mask_masks_identically_on_repeat_calls():
    """Once a message ages past the reserve floor, repeated renders must
    produce byte-identical masked content — this is what keeps the KV
    prefix stable across turns."""
    content = "y" * 1000
    rows = [{"rowid": 1, "role": "tool", "content": content, "tool_call_id": "a"}]
    cfg = TokenManagementConfig(tool_mask_head=50, tool_mask_tail=50)
    first = apply_tool_mask(rows, reserve_floor_rowid=5, cfg=cfg)
    second = apply_tool_mask(rows, reserve_floor_rowid=5, cfg=cfg)
    assert first[0]["content"] == second[0]["content"]


def test_apply_tool_mask_leaves_short_content_alone():
    rows = [{"rowid": 1, "role": "tool", "content": "short", "tool_call_id": "a"}]
    cfg = TokenManagementConfig(tool_mask_head=400, tool_mask_tail=400)
    masked = apply_tool_mask(rows, reserve_floor_rowid=5, cfg=cfg)
    assert masked[0]["content"] == "short"


def test_emergency_trim_preserves_pairing_and_never_touches_reserved():
    rows = [
        {"rowid": 1, "role": "user", "content": "a"},
        {"rowid": 2, "role": "assistant", "content": "", "tool_calls": [{"id": "t1"}]},
        {"rowid": 3, "role": "tool", "content": "x" * 500, "tool_call_id": "t1"},
        {"rowid": 4, "role": "user", "content": "b"},
        {"rowid": 5, "role": "assistant", "content": "y" * 500},
    ]
    # Reserve floor = 4: exchange [1,2,3] is eligible, [4,5] is reserved.
    trimmed = emergency_trim(rows, reserve_floor_rowid=4, target_tokens=1)
    reserved_rowids = {4, 5}
    trimmed_rowids = {m["rowid"] for m in trimmed if "rowid" in m}
    assert reserved_rowids.issubset(trimmed_rowids)
    # The eligible exchange (1,2,3) was dropped as a whole unit, not partially.
    assert not ({1, 2, 3} & trimmed_rowids)
    assert any(m["role"] == "system" and "elided" in m["content"] for m in trimmed)


def test_emergency_trim_noop_when_nothing_eligible():
    rows = [{"rowid": 5, "role": "user", "content": "a"}]
    trimmed = emergency_trim(rows, reserve_floor_rowid=1, target_tokens=0)
    assert trimmed == rows


# ---------------------------------------------------------------------------
# Memory slot merging (append-only, deduped)
# ---------------------------------------------------------------------------

def test_merge_memory_slots_is_append_only_and_dedups():
    existing = {
        "new_constraints": ["use TypeScript"],
        "new_decisions": [],
        "new_completions": [],
        "current_state": "old state",
    }
    new = {
        "new_constraints": ["use TypeScript", "no any types"],  # dup + new
        "new_decisions": ["picked Zustand"],
        "new_completions": [],
        "current_state": "new state",
    }
    merged = merge_memory_slots(existing, new)
    assert merged["new_constraints"] == ["use TypeScript", "no any types"]
    assert merged["new_decisions"] == ["picked Zustand"]
    assert merged["current_state"] == "new state"


def test_merge_memory_slots_bad_pass_cannot_erase_existing():
    existing = {
        "new_constraints": ["load-bearing constraint"],
        "new_decisions": ["important decision"],
        "new_completions": ["big feature done"],
        "current_state": "state",
    }
    # A pathological/empty summarizer result.
    merged = merge_memory_slots(existing, {})
    assert merged["new_constraints"] == ["load-bearing constraint"]
    assert merged["new_decisions"] == ["important decision"]
    assert merged["new_completions"] == ["big feature done"]
    # current_state falls back to existing when the new pass gives nothing.
    assert merged["current_state"] == "state"


def test_consolidate_slot_if_needed_trims_to_recency():
    slots = {"new_constraints": [f"c{i}" for i in range(30)], "new_decisions": [], "new_completions": [], "current_state": ""}
    consolidated = consolidate_slot_if_needed(slots, max_items=20)
    assert len(consolidated["new_constraints"]) == 20
    assert consolidated["new_constraints"][-1] == "c29"


def test_render_memory_block_omits_when_empty():
    assert render_memory_block(None) is None
    assert render_memory_block({"new_constraints": [], "new_decisions": [], "new_completions": [], "current_state": ""}) is None


def test_render_memory_block_renders_content():
    block = render_memory_block({
        "new_constraints": ["c1"], "new_decisions": [], "new_completions": [], "current_state": "s1",
    })
    assert block is not None
    assert "c1" in block and "s1" in block


# ---------------------------------------------------------------------------
# run_compaction: end-to-end against an in-memory DB
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_run_compaction_noop_below_threshold(db, session_id, token_cfg, summarizer_cfg):
    await _make_session(db, session_id)
    for i in range(6):
        await _insert_message(db, session_id, "user" if i % 2 == 0 else "assistant", "short")
    fake = FakeSummarizer()
    result = await run_compaction(session_id, db, fake, token_cfg, summarizer_cfg, context_length=1000)
    assert result is None
    assert fake.calls == 0
    row = await db.fetch_one("SELECT compaction_state FROM sessions WHERE id = :id", {"id": session_id})
    assert row["compaction_state"] == "idle"


@pytest.mark.asyncio
async def test_run_compaction_folds_old_exchanges_and_advances_watermark(
    db, session_id, token_cfg, summarizer_cfg
):
    await _make_session(db, session_id)
    # Build enough big exchanges to exceed threshold (1000 * 0.8 = 800 tokens).
    last_rowid = 0
    for i in range(10):
        last_rowid = await _insert_message(db, session_id, "user", "u" * 200)
        last_rowid = await _insert_message(db, session_id, "assistant", "a" * 200)

    fake = FakeSummarizer()
    result = await run_compaction(session_id, db, fake, token_cfg, summarizer_cfg, context_length=1000)

    assert result is not None
    assert fake.calls == 1
    assert isinstance(result, CompactionResult)
    assert result.tokens_after < result.tokens_before

    row = await db.fetch_one(
        "SELECT compacted_through_rowid, memory_slots, compaction_state FROM sessions WHERE id = :id",
        {"id": session_id},
    )
    assert row["compaction_state"] == "idle"
    assert row["compacted_through_rowid"] > 0
    assert row["compacted_through_rowid"] < last_rowid  # reserve exchange untouched
    slots = json.loads(row["memory_slots"])
    assert slots["current_state"] == "wiring up settings UI"


@pytest.mark.asyncio
async def test_run_compaction_summarizer_failure_leaves_state_untouched(
    db, session_id, token_cfg, summarizer_cfg
):
    await _make_session(db, session_id)
    # Seed pre-existing memory to prove it survives a failed pass byte-for-byte.
    existing_slots = {"new_constraints": ["keep me"], "new_decisions": [], "new_completions": [], "current_state": "keep this too"}
    await db.execute(
        "UPDATE sessions SET memory_slots = :s, compacted_through_rowid = 0 WHERE id = :id",
        {"s": json.dumps(existing_slots), "id": session_id},
    )
    for i in range(10):
        await _insert_message(db, session_id, "user", "u" * 200)
        await _insert_message(db, session_id, "assistant", "a" * 200)

    fake = FakeSummarizer(fail=True)
    result = await run_compaction(session_id, db, fake, token_cfg, summarizer_cfg, context_length=1000)

    assert result is None
    row = await db.fetch_one(
        "SELECT compacted_through_rowid, memory_slots, compaction_state FROM sessions WHERE id = :id",
        {"id": session_id},
    )
    assert row["compaction_state"] == "idle"
    assert row["compacted_through_rowid"] == 0
    assert json.loads(row["memory_slots"]) == existing_slots


@pytest.mark.asyncio
async def test_run_compaction_cas_lock_only_one_of_two_concurrent_callers_proceeds(
    db, session_id, token_cfg, summarizer_cfg
):
    await _make_session(db, session_id)
    for i in range(10):
        await _insert_message(db, session_id, "user", "u" * 200)
        await _insert_message(db, session_id, "assistant", "a" * 200)

    fake_a = FakeSummarizer()
    fake_b = FakeSummarizer()
    results = await asyncio.gather(
        run_compaction(session_id, db, fake_a, token_cfg, summarizer_cfg, context_length=1000),
        run_compaction(session_id, db, fake_b, token_cfg, summarizer_cfg, context_length=1000),
    )
    succeeded = [r for r in results if r is not None]
    # Exactly one of the two concurrent passes should have actually run the
    # summarizer and committed a result — the other must see the lock held.
    assert len(succeeded) == 1
    assert fake_a.calls + fake_b.calls == 1


@pytest.mark.asyncio
async def test_run_compaction_reclaims_stale_lock(db, session_id, token_cfg, summarizer_cfg):
    await _make_session(db, session_id)
    for i in range(10):
        await _insert_message(db, session_id, "user", "u" * 200)
        await _insert_message(db, session_id, "assistant", "a" * 200)

    # Simulate a daemon crash mid-pass: state stuck at 'running' with a
    # long-past timestamp.
    stale_time = (datetime.now(timezone.utc) - timedelta(seconds=1000)).strftime("%Y-%m-%d %H:%M:%S")
    await db.execute(
        "UPDATE sessions SET compaction_state = 'running', compaction_started_at = :t WHERE id = :id",
        {"t": stale_time, "id": session_id},
    )

    fake = FakeSummarizer()
    result = await run_compaction(session_id, db, fake, token_cfg, summarizer_cfg, context_length=1000)
    assert result is not None
    assert fake.calls == 1


@pytest.mark.asyncio
async def test_run_compaction_does_not_reclaim_fresh_lock(db, session_id, token_cfg, summarizer_cfg):
    await _make_session(db, session_id)
    for i in range(10):
        await _insert_message(db, session_id, "user", "u" * 200)
        await _insert_message(db, session_id, "assistant", "a" * 200)

    await db.execute(
        "UPDATE sessions SET compaction_state = 'running', compaction_started_at = CURRENT_TIMESTAMP "
        "WHERE id = :id",
        {"id": session_id},
    )
    fake = FakeSummarizer()
    result = await run_compaction(session_id, db, fake, token_cfg, summarizer_cfg, context_length=1000)
    assert result is None
    assert fake.calls == 0


@pytest.mark.asyncio
async def test_run_compaction_disabled_summarizer_is_noop(db, session_id, token_cfg):
    await _make_session(db, session_id)
    for i in range(10):
        await _insert_message(db, session_id, "user", "u" * 200)
        await _insert_message(db, session_id, "assistant", "a" * 200)

    cfg = SummarizerConfig(enabled=False)
    fake = FakeSummarizer()
    result = await run_compaction(session_id, db, fake, token_cfg, cfg, context_length=1000)
    assert result is None
    assert fake.calls == 0
