import asyncio
import numpy as np
import tempfile
import os
import sys
import time
import json
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


def test_decide_latency_empty_graph():
    ap = AdaptivePathway()
    ap._warm_ready = True
    from adaptive_pathway.types import SessionState
    from adaptive_pathway.decision.in_session import InSessionBandit
    from adaptive_pathway.decision.selector import ActionSelector
    in_session = InSessionBandit(ap.config)
    selector = ActionSelector(
        ap._ensemble, in_session, ap._novelty, ap.config,
        ap._get_domain, ap._get_edges, ap._bucketer, ap._hasher,
        action_history=[], novelty_history=[],
        bleed=ap._bleed, ttl=ap._ttl,
    )
    state = SessionState(session_id="bench", mode="thought_partner",
                         in_session=in_session, selector=selector,
                         opened_at=time.strftime("%Y-%m-%dT%H:%M:%SZ"))
    ap._sessions["bench"] = state

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    latencies = []
    for _ in range(100):
        t0 = time.perf_counter()
        ap.decide("bench", ctx, ["action_a", "action_b", "action_c"])
        latencies.append((time.perf_counter() - t0) * 1000)

    avg_latency = np.mean(latencies)
    p99_latency = np.percentile(latencies, 99)
    print(f"\ndecide() avg: {avg_latency:.3f}ms, p99: {p99_latency:.3f}ms")
    assert avg_latency < 10.0, f"Average latency {avg_latency:.3f}ms exceeds 10ms target"


def test_decide_latency_populated_graph():
    # The empty-graph benchmark above never exercises the per-edge Thompson
    # sampling loop (it short-circuits with no edges), so it can't catch
    # regressions in the hot path itself. With real edges + embeddings
    # present, decide() previously took ~330ms/call (numpy's default
    # SVD-based multivariate_normal, redrawn per edge) against a 10ms
    # budget — this guards against that regressing.
    from adaptive_pathway.types import SessionState, EdgeInfo
    from adaptive_pathway.decision.in_session import InSessionBandit
    from adaptive_pathway.decision.selector import ActionSelector

    ap = AdaptivePathway()
    ap._warm_ready = True

    rng = np.random.default_rng(0)
    domains = ["python", "javascript", "rust", "sql"]
    for i in range(50):
        bucket = ap._bucketer.get_bucket(f"primitive_{i}")
        emb = rng.standard_normal(384).astype(np.float32)
        emb /= np.linalg.norm(emb)
        edge = EdgeInfo(id=f"edge_{i}", semantic_primitive=f"primitive_{i}",
                        domain_id=domains[i % 4], domain=domains[i % 4],
                        confidence=float(rng.uniform(0.3, 0.95)), tier="hot",
                        frequency=int(rng.integers(0, 50)), embedding=emb)
        ap._edge_index.setdefault(bucket, []).append(edge)
    all_edges = [e for bucket_edges in ap._edge_index.values() for e in bucket_edges]
    ap._tiered.warm_from_db(all_edges, [(e.id, e.embedding) for e in all_edges])

    in_session = InSessionBandit(ap.config)
    selector = ActionSelector(
        ap._ensemble, in_session, ap._novelty, ap.config,
        ap._get_domain, ap._get_edges, ap._bucketer, ap._hasher,
        action_history=[], novelty_history=[],
        bleed=ap._bleed, ttl=ap._ttl,
    )
    selector.set_nudge(ap._nudge)
    state = SessionState(session_id="bench", mode="thought_partner",
                         in_session=in_session, selector=selector,
                         opened_at=time.strftime("%Y-%m-%dT%H:%M:%SZ"))
    ap._sessions["bench"] = state

    ctx = rng.standard_normal(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    actions = [f"edge_{i}" for i in range(50)]

    latencies = []
    for _ in range(50):
        t0 = time.perf_counter()
        ap.decide("bench", ctx, actions)
        latencies.append((time.perf_counter() - t0) * 1000)

    avg_latency = np.mean(latencies)
    print(f"\ndecide() [populated graph] avg: {avg_latency:.3f}ms")
    assert avg_latency < 10.0, f"Average latency {avg_latency:.3f}ms exceeds 10ms target"


@pytest.mark.asyncio
async def test_session_open_latency(db_path):
    ap = AdaptivePathway(db_path=db_path)

    t0 = time.perf_counter()
    await ap.session_open("bench")
    elapsed = (time.perf_counter() - t0) * 1000
    print(f"\nsession_open() cold start: {elapsed:.3f}ms")
    assert elapsed < 2000

    await ap.session_close("bench")


@pytest.mark.asyncio
async def test_crash_recovery_rehydration(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(100):
        await ap.record_outcome("s1", f"action_{i}", 0.5 if i % 2 == 0 else -0.3, ctx)

    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    assert ap2._warm_ready
    assert len(ap2._action_history) == 100
    assert len(ap2._novelty_history) == 100
    result = ap2.decide("s2", ctx, ["action_0", "action_50", "action_99"])
    assert result.confidence > 0.3
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_crash_recovery_bandit_reset(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    for _ in range(20):
        await ap.record_outcome("s1", "action_x", 1.0, ctx)

    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s1")
    assert ap2._sessions["s1"].in_session.call_count == 0
    assert ap2._sessions["s1"].in_session.mix_weight == 0.0
    await ap2.session_close("s1")


@pytest.mark.asyncio
async def test_cold_start_empty_db(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    # Truly cold: nothing recorded yet — maximal novelty, neutral confidence.
    result = ap.decide("s1", ctx, ["a", "b", "c"])
    assert result.confidence == 0.5
    assert result.novelty == 1.0

    # Outcomes now genuinely warm the graph (each creates/updates a real
    # edge carrying this context), so revisiting the SAME context must read
    # as less-than-maximally novel — the old expectation that novelty stays
    # pinned at 1.0 forever was itself the no-edges-ever-created bug.
    for i in range(5):
        await ap.record_outcome("s1", "a", 0.3, ctx)
        result = ap.decide("s1", ctx, ["a", "b", "c"])
        assert 0.0 <= result.novelty <= 1.0
    assert result.novelty < 1.0

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_purge_maintenance_large_data(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(200):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    assert len(ap._action_history) == 200

    await ap.run_maintenance()
    assert len(ap._action_history) == 200

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_maintenance_row_based_retention(db_path):
    import sqlalchemy as sa
    from adaptive_pathway.storage.database import ActionHistoryModel, NoveltyHistoryModel

    # Force a tiny retention window: entropy_window(1) * entropy_stride(1) * 10 = 10 rows
    ap = AdaptivePathway(db_path=db_path, **{
        "plateau_risk.entropy_window": 1,
        "plateau_risk.entropy_stride": 1,
    })
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(25):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    await ap.run_maintenance()

    async with ap._engine.begin() as conn:
        action_count = (await conn.execute(
            sa.select(sa.func.count()).select_from(ActionHistoryModel))).scalar()
        novelty_count = (await conn.execute(
            sa.select(sa.func.count()).select_from(NoveltyHistoryModel))).scalar()

    # Row-based retention keeps the most recent 10 rows, not a time window.
    assert action_count == 10
    assert novelty_count == 10

    await ap.session_close("s1")



@pytest.mark.asyncio
async def test_engine_reopen_preserves_graph(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_outcome("s1", "important_action", 1.0, ctx)
    await ap.record_outcome("s1", "bad_action", -0.8, ctx, error_type="crash")

    assert ap._ttl.is_expired("bad_action")
    assert len(ap._action_history) == 2

    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s1")

    assert len(ap2._action_history) == 2

    await ap2.session_close("s1")


@pytest.mark.asyncio
async def test_goose_e2e_smoke(proxy_env):
    import json
    from mcp.client.session import ClientSession
    from mcp.client.stdio import stdio_client, StdioServerParameters

    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "adaptive_pathway.mcp_server"],
        env=proxy_env,
    )

    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()

            for turn in range(5):
                dr = await session.call_tool("decide", {
                    "session_id": "e2e_test",
                    "available_actions": ",".join(f"tool_{i}" for i in range(8)),
                })
                raw = dr.content[0].text
                assert "hints" in raw
                assert "confidence" in raw

                chosen = f"tool_{turn % 4}"
                await session.call_tool("record_outcome", {
                    "session_id": "e2e_test",
                    "action_id": chosen,
                    "reward": 0.5 if turn < 4 else -0.3,
                })

                state = await session.call_tool("get_state", {
                    "session_id": "e2e_test",
                })
                assert state is not None

            state_final = await session.call_tool("get_state", {
                "session_id": "e2e_test",
            })
            assert "action_history_len" in state_final.content[0].text

            edges = await session.call_tool("list_edges", {
                "page": 1, "per_page": 10,
            })
            assert edges is not None

            domains = await session.call_tool("list_domains", {})
            assert domains is not None

            toggle = await session.call_tool("toggle_suggestions", {
                "session_id": "e2e_test",
                "paused": True,
            })
            data = json.loads(toggle.content[0].text)
            assert data["paused"] is True

            toggle2 = await session.call_tool("toggle_suggestions", {
                "session_id": "e2e_test",
                "paused": False,
            })
            data2 = json.loads(toggle2.content[0].text)
            assert data2["paused"] is False

            health = await session.call_tool("health_check", {})
            assert "[" in health.content[0].text


@pytest.mark.asyncio
async def test_graceful_degradation_empty_graph(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    result = ap.decide("s1", ctx, [])
    assert result.hints == []
    assert result.confidence == 0.5

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_annotation_and_health_loop(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_annotation("s1", {
        "type": "keep_this", "edge_id": "edge_1",
        "context_embedding": ctx, "intensity": 0.9,
    })
    await ap.record_annotation("s1", {
        "type": "dont_do_again", "edge_id": "edge_2",
        "context_embedding": ctx, "intensity": 0.8,
    })

    state = ap.get_state()
    assert state["warm_ready"] is True

    health = ap.health_check()
    assert isinstance(health, list)

    graph_health = ap.get_graph_health()
    assert graph_health is not None

    await ap.session_close("s1")
