import asyncio
import numpy as np
import tempfile
import os
import pytest
from adaptive_pathway import AdaptivePathway
from adaptive_pathway.types import SchismState


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
async def test_two_sessions_independent_bandits(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("s1", mode="thought_partner")
    await ap.session_open("s2", mode="thought_partner")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for _ in range(10):
        await ap.record_outcome("s1", "action_a", 1.0, ctx)

    for _ in range(3):
        await ap.record_outcome("s2", "action_b", -1.0, ctx)

    s1 = ap._sessions["s1"]
    s2 = ap._sessions["s2"]
    assert s1.in_session.call_count == 10
    assert s2.in_session.call_count == 3

    result_s1 = ap.decide("s1", ctx, ["action_a", "action_b"])
    result_s2 = ap.decide("s2", ctx, ["action_a", "action_b"])
    assert result_s1.in_session.mix_weight > result_s2.in_session.mix_weight

    await ap.session_close("s1")
    await ap.session_close("s2")


@pytest.mark.asyncio
async def test_sessions_share_histories(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("s1")
    await ap.session_open("s2")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_outcome("s1", "action_1", 1.0, ctx)
    await ap.record_outcome("s2", "action_2", 0.5, ctx)
    await ap.record_outcome("s1", "action_3", 0.0, ctx)

    assert len(ap._action_history) == 3
    assert len(ap._novelty_history) == 3

    await ap.session_close("s1")
    await ap.session_close("s2")


@pytest.mark.asyncio
async def test_session_close_resets_bandit(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("s1")
    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    for _ in range(15):
        await ap.record_outcome("s1", "action_a", 1.0, ctx)
    assert ap._sessions["s1"].in_session.call_count == 15

    await ap.session_close("s1")

    await ap.session_open("s1")
    assert ap._sessions["s1"].in_session.call_count == 0

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_three_concurrent_sessions(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("s_a")
    await ap.session_open("s_b")
    await ap.session_open("s_c")

    assert len(ap._sessions) == 3
    assert ap._sessions["s_a"].in_session is not None
    assert ap._sessions["s_b"].in_session is not None
    assert ap._sessions["s_c"].in_session is not None

    assert ap._sessions["s_a"].in_session is not ap._sessions["s_b"].in_session

    await ap.session_close("s_a")
    await ap.session_close("s_b")
    await ap.session_close("s_c")
    assert len(ap._sessions) == 0


@pytest.mark.asyncio
async def test_sessions_different_modes(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("agent_sess", mode="agent")
    await ap.session_open("tp_sess", mode="thought_partner")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    agent_lam = ap._novelty.get_lambda_for_mode("agent")
    tp_lam = ap._novelty.get_lambda_for_mode("thought_partner")
    assert agent_lam < tp_lam

    await ap.session_close("agent_sess")
    await ap.session_close("tp_sess")


@pytest.mark.asyncio
async def test_schism_during_multi_session(db_path):
    ap = AdaptivePathway(db_path=db_path)
    ensemble = ap._ensemble
    ensemble.schism_state = SchismState.REVIEWING

    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    result = ap.decide("s1", ctx, ["action_x"])
    assert result.confidence == 0.5
    assert result.novelty == 0.0
    assert result.hints == []

    ensemble.schism_state = SchismState.NONE
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_toggle_suggestions_multi_session(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("s1")
    await ap.session_open("s2")

    ap.toggle_suggestions("s1", True)
    assert ap._sessions["s1"].suggestions_paused is True
    assert ap._sessions["s2"].suggestions_paused is False

    ap.toggle_suggestions("s1", False)
    ap.toggle_suggestions("s2", True)
    assert ap._sessions["s1"].suggestions_paused is False
    assert ap._sessions["s2"].suggestions_paused is True

    await ap.session_close("s1")
    await ap.session_close("s2")


@pytest.mark.asyncio
async def test_session_close_with_annotations(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("s1")
    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    ap._sessions["s1"].annotations_deferred = [
        {"type": "keep_this", "edge_id": "e1", "context_embedding": ctx, "intensity": 0.5},
        {"type": "dont_do_again", "edge_id": "e2", "context_embedding": ctx, "intensity": 0.8},
    ]

    await ap.session_close("s1")
    assert "s1" not in ap._sessions
