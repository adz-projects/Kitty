import asyncio
import numpy as np
import tempfile
import os
import time
import pytest
from adaptive_pathway import AdaptivePathway


@pytest.fixture
def db_path():
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    yield path
    try:
        os.remove(path)
    except PermissionError:
        pass


@pytest.mark.asyncio
async def test_action_history_persistence_round_trip(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(50):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    assert len(ap._action_history) == 50
    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    assert ap2._warm_ready is True
    assert len(ap2._action_history) == 50
    assert set(ap2._action_history) == set(f"action_{i}" for i in range(50))
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_action_history_2000_cap(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(2500):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    assert len(ap._action_history) == 2000
    assert ap._action_history[0] == "action_500"
    assert ap._action_history[-1] == "action_2499"

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_novelty_history_persistence(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(30):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    assert len(ap._novelty_history) == 30
    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    assert len(ap2._novelty_history) == 30
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_blended_edge_log_persisted(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_outcome(
        "s1", "blended_action", 0.7, ctx,
        is_blended=True, blend_edge_ids=["edge_a", "edge_b"],
    )

    assert len(ap._action_history) == 1
    assert "blended:" in ap._action_history[0]

    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    assert len(ap2._action_history) == 1
    assert "blended:" in ap2._action_history[0]
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_annotation_cache_survives_sessions(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_annotation("s1", {
        "type": "keep_this", "edge_id": "edge_x",
        "context_embedding": ctx, "intensity": 0.6,
    })

    assert len(ap._annotations_cache) == 1
    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    assert len(ap2._annotations_cache) == 0
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_maintenance_does_not_truncate_memory(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(25):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    before = len(ap._action_history)
    await ap.run_maintenance()
    after = len(ap._action_history)
    assert before == after

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_co_selection_persistence(db_path):
    # Co-selection tracking is per-session state (SessionState.co_selected),
    # not a global engine dict — each session flushes only its own pairs to
    # the co_selection_log table on close, so concurrent sessions can't
    # clobber each other's data.
    import sqlalchemy as sa
    from adaptive_pathway.storage.database import CoSelectionLogModel

    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ap._sessions["s1"].co_selected = {"prim_a": {"prim_b", "prim_c"}, "prim_b": {"prim_c"}}

    await ap.session_close("s1")
    assert "s1" not in ap._sessions

    async with ap._engine.begin() as conn:
        count = (await conn.execute(
            sa.select(sa.func.count()).select_from(CoSelectionLogModel))).scalar()
    assert count == 3


@pytest.mark.asyncio
async def test_record_outcome_updates_action_history(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(5):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    assert ap._action_history[-5:] == [f"action_{i}" for i in range(5)]

    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    assert set(ap2._action_history) == set(f"action_{i}" for i in range(5))
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_reopen_session_with_prior_history(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_outcome("s1", "action_1", 1.0, ctx)
    await ap.record_outcome("s1", "action_2", -0.5, ctx)
    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s1")
    assert ap2._warm_ready
    assert len(ap2._action_history) == 2
    assert len(ap2._novelty_history) == 2

    result = ap2.decide("s1", ctx, ["action_1", "action_2", "action_3"])
    assert result.confidence == 0.5 or result.confidence > 0.4

    await ap2.session_close("s1")
