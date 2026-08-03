import asyncio
import numpy as np
import tempfile
import os
import time
import pytest
import sqlalchemy as sa
from adaptive_pathway import AdaptivePathway
from adaptive_pathway.storage.database import EdgeModel, EnsembleStateModel, NodeModel
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


def test_engine_constructs():
    ap = AdaptivePathway()
    assert ap._warm_ready is False
    assert len(ap._sessions) == 0
    assert ap._ensemble is not None
    assert ap._novelty is not None
    assert ap._nudge is not None
    assert ap._detector is not None
    assert ap._ttl is not None
    assert ap._bleed is not None


@pytest.mark.asyncio
async def test_session_lifecycle(db_path):
    ap = AdaptivePathway(db_path=db_path)
    state = await ap.session_open("s1", mode="thought_partner")
    assert state.session_id == "s1"
    assert state.mode == "thought_partner"
    assert ap._warm_ready is True

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    result = ap.decide("s1", ctx, ["action_1", "action_2"])
    assert result.confidence == 0.5
    assert result.novelty == 1.0
    assert result.is_flow_state is False

    await ap.record_outcome("s1", "action_1", 1.0, ctx)
    await ap.session_close("s1")
    assert "s1" not in ap._sessions


@pytest.mark.asyncio
async def test_multi_session_isolation(db_path):
    ap = AdaptivePathway(db_path=db_path)

    await ap.session_open("s1", mode="thought_partner")
    await ap.session_open("s2", mode="thought_partner")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_outcome("s1", "action_1", 1.0, ctx)
    await ap.record_outcome("s2", "action_2", -1.0, ctx)
    await ap.record_outcome("s1", "action_1", 1.0, ctx)

    s1 = ap._sessions["s1"]
    s2 = ap._sessions["s2"]
    assert s1.in_session.call_count == 2
    assert s2.in_session.call_count == 1

    assert len(ap._action_history) == 3

    await ap.session_close("s1")
    await ap.session_close("s2")


@pytest.mark.asyncio
async def test_record_annotation(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_annotation("s1", {
        "type": "keep_this",
        "edge_id": "edge_1",
        "context_embedding": ctx,
        "intensity": 0.7,
    })
    assert ap._detector._example_count == 1

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_ttl_integration(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    assert ap._ttl.is_expired("some_edge") is False
    await ap.record_error("some_edge", "permission_denied")
    assert ap._ttl.is_expired("some_edge") is True

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_bleed_scoring():
    ap = AdaptivePathway()
    assert ap._bleed.bleed_score("python", "python") == 1.0
    cross = ap._bleed.bleed_score("python", "javascript")
    assert 0.0 < cross < 1.0


@pytest.mark.asyncio
async def test_negative_reward_triggers_ttl_only_with_explicit_crash(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    # Row 7 of 82inefficiencies.md: a plain negative reward must NOT be
    # assumed to be a crash (the backstop's context-free negative reward was
    # auto-expiring every failed action for 24h).
    await ap.record_outcome("s1", "plain_failure", -0.8, ctx)
    assert ap._ttl.is_expired("plain_failure") is False

    await ap.record_outcome("s1", "crash_failure", -0.8, ctx, error_type="crash")
    assert ap._ttl.is_expired("crash_failure") is True

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_toggle_suggestions(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    result = ap.toggle_suggestions("s1", True)
    assert result is True
    assert ap._sessions["s1"].suggestions_paused is True

    result = ap.toggle_suggestions("s1", False)
    assert result is True
    assert ap._sessions["s1"].suggestions_paused is False

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_select_decide_twice_blended_outcome(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    result1 = ap.decide("s1", ctx, ["action_a", "action_b"])
    assert result1.confidence == 0.5

    await ap.record_outcome("s1", "action_a", 0.5, ctx, is_blended=True, blend_edge_ids=["ea", "eb"])
    assert len(ap._action_history) == 1
    assert "blended:" in ap._action_history[0]

    await ap.session_close("s1")


# ─── Phase 3 Tests ────────────────────────────────────────

@pytest.mark.asyncio
async def test_get_state_full(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    state = ap.get_state()
    assert state["warm_ready"] is True
    assert "sessions" in state
    assert "domains" in state
    assert "feature_utilization" in state
    assert "novelty_lambda" in state
    assert state["nudge_active"] is False
    await ap.session_close("s1")


# ─── embedding_info / get_state()["embedding"] ─────────────────────────────


@pytest.mark.asyncio
async def test_embedding_info_untried_before_any_embed_call(db_path):
    ap = AdaptivePathway(db_path=db_path)
    info = ap.embedding_info()
    assert info["backend"] == "untried"
    assert info["model"] == ap._embedder.ollama_model
    assert info["url"] == ap._embedder.ollama_url


@pytest.mark.asyncio
async def test_embedding_info_reports_ollama_backend(db_path):
    ap = AdaptivePathway(db_path=db_path)
    ap._embedder._ollama_available = True
    assert ap.embedding_info()["backend"] == "ollama"


@pytest.mark.asyncio
async def test_embedding_info_reports_hashing_fallback_backend(db_path):
    ap = AdaptivePathway(db_path=db_path)
    ap._embedder._ollama_available = False
    assert ap.embedding_info()["backend"] == "hashing"


@pytest.mark.asyncio
async def test_get_state_includes_embedding_block(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    state = ap.get_state()
    assert "embedding" in state
    assert set(state["embedding"].keys()) == {"backend", "model", "url", "failed_decodes"}
    await ap.session_close("s1")


# ─── ensemble_state persistence (learned bandit weights survive restart) ──


async def _ensemble_state_rows(ap):
    async with ap._engine.begin() as conn:
        result = await conn.execute(sa.select(EnsembleStateModel))
        return list(result)


@pytest.mark.asyncio
async def test_record_outcome_persists_ensemble_state_row(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    await ap.record_outcome("s1", "action_a", 1.0, ctx)
    rows = await _ensemble_state_rows(ap)
    # Models 0-4 (4 standard + IG) are learnable; model 5 (Paradigm
    # Challenge) has no weights, so exactly 5 rows for the one touched bucket.
    assert len(rows) == 5
    assert {r.model_index for r in rows} == {0, 1, 2, 3, 4}
    bucket = ap._bucketer.get_bucket("action_a")
    assert all(r.action_id == bucket for r in rows)
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_record_outcome_upserts_rather_than_duplicates(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    await ap.record_outcome("s1", "action_a", 1.0, ctx)
    await ap.record_outcome("s1", "action_a", -1.0, ctx)
    rows = await _ensemble_state_rows(ap)
    # A second outcome on the same bucket must update the existing 5 rows,
    # not insert 5 more.
    assert len(rows) == 5
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_ensemble_state_survives_restart(db_path):
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)

    ap1 = AdaptivePathway(db_path=db_path)
    await ap1.session_open("s1")
    # The IG model (index 4) updates unconditionally on every call (no
    # bootstrap gate), so a single outcome is enough to deterministically
    # diverge it from its identity-matrix prior.
    await ap1.record_outcome("s1", "action_a", 1.0, ctx)
    bucket = ap1._bucketer.get_bucket("action_a")
    ig_state_before = ap1._ensemble.models[4].get_state(bucket)
    await ap1.session_close("s1")

    # A brand-new engine instance against the same db_path — simulates a
    # process restart (sidecar or MCP extension relaunch).
    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    ig_state_after = ap2._ensemble.models[4].get_state(bucket)

    assert ig_state_after["A_inv"] == ig_state_before["A_inv"]
    assert ig_state_after["b"] == ig_state_before["b"]
    # And it must actually be non-default — otherwise this would pass
    # vacuously even if restore silently no-op'd. Compare the whole matrix
    # (not one row/column) since a single hashed context vector can leave
    # individual rows untouched by the Sherman-Morrison update.
    d = len(ig_state_after["A_inv"])
    identity = [[1.0 if i == j else 0.0 for j in range(d)] for i in range(d)]
    assert ig_state_after["A_inv"] != identity
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_restore_ensemble_state_skips_stale_shape(db_path):
    ap = AdaptivePathway(db_path=db_path)

    class FakeRow:
        def __init__(self, model_index, action_id, a_inv, b):
            self.model_index = model_index
            self.action_id = action_id
            self.A_inv = a_inv
            self.b_vector = b

    import json

    # Wrong dimension (e.g. `thompson.feature_buckets` changed since this
    # row was written) — must be skipped, not applied or crash.
    bad_row = FakeRow(0, 0, json.dumps([[1.0, 0.0], [0.0, 1.0]]).encode(), json.dumps([0.0, 0.0]).encode())
    before = ap._ensemble.models[0].get_state(0)
    ap._restore_ensemble_state([bad_row])
    after = ap._ensemble.models[0].get_state(0)
    assert after == before

    # Out-of-range action_id (e.g. `max_action_buckets` shrank) — skipped too.
    oob_row = FakeRow(0, ap._ensemble.n_actions + 10, b"[]", b"[]")
    ap._restore_ensemble_state([oob_row])  # must not raise


@pytest.mark.asyncio
async def test_record_annotation_persists_ensemble_state_when_reward_nonzero(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    await ap.record_annotation("s1", {
        "type": "keep_this", "edge_id": "edge_a", "context_embedding": ctx.tolist(),
    })
    rows = await _ensemble_state_rows(ap)
    assert len(rows) == 5
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_record_annotation_no_crash_without_context_embedding(db_path):
    # Regression test: this is exactly Kitty's copy-button micro_positive
    # call shape (no context_embedding, no context) — used to raise
    # UnboundLocalError on `reward_weight` before both were given defaults.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    await ap.record_annotation("s1", {
        "type": "micro_positive", "edge_id": "edge_a", "action_id": None, "intensity": 0.6,
    })
    rows = await _ensemble_state_rows(ap)
    assert rows == []  # no context -> no ensemble update -> nothing to persist
    await ap.session_close("s1")


# ─── Graph (edges/nodes/domains) actually gets written ─────────────────────
# Root cause of the "list_edges/list_domains empty after a week of use" bug:
# NOTHING anywhere inserted into edges/nodes (confirmed against the live
# production db: 88 action_history rows, 30 ensemble_state rows, 0 edges).


async def _edge_rows(ap):
    async with ap._engine.begin() as conn:
        result = await conn.execute(sa.select(EdgeModel))
        return list(result)


@pytest.mark.asyncio
async def test_record_outcome_creates_edge_and_node_rows(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    await ap.record_outcome("s1", "shell", 1.0, ctx)
    rows = await _edge_rows(ap)
    assert len(rows) == 1
    assert rows[0].semantic_primitive == "shell"
    assert rows[0].frequency == 1
    async with ap._engine.begin() as conn:
        result = await conn.execute(sa.select(NodeModel))
        nodes = list(result)
    assert len(nodes) == 1
    assert rows[0].source_node_id == nodes[0].id
    assert nodes[0].context_embedding is not None
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_repeat_outcome_bumps_frequency_not_duplicate(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    conf_after = []
    for _ in range(3):
        await ap.record_outcome("s1", "shell", 1.0, ctx)
        rows = await _edge_rows(ap)
        conf_after.append(rows[0].confidence)
    rows = await _edge_rows(ap)
    assert len(rows) == 1
    assert rows[0].frequency == 3
    # Positive outcomes must raise confidence monotonically...
    assert conf_after[0] < conf_after[1] < conf_after[2]
    # ...and after `cooldown.provisional_successes` uses with >0.5 confidence
    # the edge graduates out of provisional.
    assert rows[0].status == "established"
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_record_annotation_creates_style_edge(db_path):
    # Style edges are born ONLY through annotations (they never pass through
    # record_outcome) — the exact shape the per-turn nudge tells the model
    # to use for response-style learning.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    await ap.record_annotation("s1", {
        "type": "keep_this", "edge_id": "style:critique:structural",
        "context_embedding": ctx.tolist(),
    })
    rows = await _edge_rows(ap)
    assert len(rows) == 1
    assert rows[0].semantic_primitive == "style:critique:structural"
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_list_edges_and_domains_nonempty_after_outcome(db_path):
    # The user-visible symptom: list_domains() == [] and list_edges empty
    # despite real usage.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    await ap.record_outcome("s1", "shell", 1.0, ctx)
    listed = ap.list_edges()
    assert listed["total"] == 1
    assert listed["edges"][0]["semantic_primitive"] == "shell"
    domains = ap.list_domains()
    assert len(domains) >= 1
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_edges_survive_restart(db_path):
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    ap1 = AdaptivePathway(db_path=db_path)
    await ap1.session_open("s1")
    await ap1.record_outcome("s1", "shell", 1.0, ctx)
    await ap1.record_annotation("s1", {
        "type": "keep_this", "edge_id": "style:explain:thorough",
        "context_embedding": ctx.tolist(),
    })
    await ap1.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    listed = ap2.list_edges()
    primitives = {e["semantic_primitive"] for e in listed["edges"]}
    assert primitives == {"shell", "style:explain:thorough"}
    assert len(ap2.list_domains()) >= 1
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_delete_edge_persists_across_restart(db_path):
    ctx = np.random.default_rng(0).standard_normal(384).astype(np.float32)
    ap1 = AdaptivePathway(db_path=db_path)
    await ap1.session_open("s1")
    await ap1.record_outcome("s1", "shell", 1.0, ctx)
    edge_id = ap1.list_edges()["edges"][0]["id"]
    assert await ap1.delete_edge(edge_id) is True
    await ap1.session_close("s1")

    # Without the DB delete, the edge would resurrect here from _warm_data.
    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s2")
    assert ap2.list_edges()["total"] == 0
    await ap2.session_close("s2")


@pytest.mark.asyncio
async def test_get_metrics_structure(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    metrics = ap.get_metrics(time_range="7d", domain="test")
    m = metrics["metrics"]
    assert "total_actions_logged" in m
    assert "total_edges_in_memory" in m
    assert "confidence_distribution" in m
    assert "domain_usage" in m
    assert "top_overridden_edges" in m
    assert metrics["metrics"]["time_range"] == "7d"
    assert metrics["metrics"]["domain_filter"] == "test"
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_list_edges_pagination(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    result = ap.list_edges(page=1, per_page=5)
    assert "edges" in result
    assert "total" in result
    assert "page" in result
    assert "pages" in result
    assert result["page"] == 1
    assert isinstance(result["edges"], list)
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_get_edge_by_id(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    edge = ap.get_edge("nonexistent")
    assert edge is None
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_update_edge_in_memory(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    assert await ap.update_edge("nonexistent", {"confidence": 0.9}) is False
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_delete_edge_moves_to_cold(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    await ap.record_outcome("s1", "action_1", 1.0, ctx)

    all_edges = [e for bucket_edges in ap._edge_index.values() for e in bucket_edges]
    assert all_edges, "record_outcome should have created an edge"
    edge_id = all_edges[0].id

    result = await ap.delete_edge(edge_id)
    assert result is True
    assert all(
        e.id != edge_id for bucket_edges in ap._edge_index.values() for e in bucket_edges
    )

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_delete_edge_nonexistent_returns_false(db_path):
    # Regression test: `delete_edge` used to unconditionally return True even
    # when nothing matched, so a caller (the sidecar's DELETE /edges/{id})
    # could never distinguish a real delete from a no-op.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    result = await ap.delete_edge("nonexistent")
    assert result is False
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_list_annotations_filtering(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_annotation("s1", {
        "type": "keep_this",
        "edge_id": "edge_a",
        "context_embedding": ctx,
        "intensity": 0.5,
    })
    await ap.record_annotation("s1", {
        "type": "dont_do_again",
        "edge_id": "edge_b",
        "context_embedding": ctx,
        "intensity": 0.8,
    })

    result_all = ap.list_annotations()
    assert result_all["total"] == 2

    result_filtered = ap.list_annotations(annotation_type="keep_this")
    assert result_filtered["total"] == 1
    assert result_filtered["annotations"][0]["annotation_type"] == "keep_this"

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_list_domains_and_update(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    domains = ap.list_domains()
    assert isinstance(domains, list)

    result = ap.update_domain("nonexistent_domain", {"name": "new_name"})
    assert result is False

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_reset_domain(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    result_soft = await ap.reset_domain("test_domain", mode="soft")
    assert result_soft is True
    result_hard = await ap.reset_domain("test_domain", mode="hard")
    assert result_hard is True
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_export_import_graph(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    exported = ap.export_graph()
    assert "edges" in exported
    assert "domains" in exported
    assert exported["version"] == "0.1.0"

    result = await ap.import_graph(exported, mode="merge")
    assert result is True

    result_replace = await ap.import_graph(exported, mode="replace_all")
    assert result_replace is True

    await ap.session_close("s1")


def _edge_by_primitive(ap, primitive):
    for bucket_edges in ap._edge_index.values():
        for e in bucket_edges:
            if e.semantic_primitive == primitive:
                return e
    return None


@pytest.mark.asyncio
async def test_import_graph_replace_all_clears_db_not_just_memory(db_path):
    # Regression test: replace_all used to clear only the in-memory index —
    # old DB rows were still there, so they silently resurrected into
    # _edge_index the next time the engine restarted.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    await ap.record_outcome("s1", "will_be_replaced", 1.0, ctx)
    assert _edge_by_primitive(ap, "will_be_replaced") is not None

    ok = await ap.import_graph({"edges": [{
        "id": "survivor",
        "semantic_primitive": "prim_survivor",
        "confidence": 0.9,
    }]}, mode="replace_all")
    assert ok is True
    assert _edge_by_primitive(ap, "will_be_replaced") is None
    assert ap.get_edge("survivor") is not None

    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s1")
    assert _edge_by_primitive(ap2, "will_be_replaced") is None
    assert ap2.get_edge("survivor") is not None
    await ap2.session_close("s1")


@pytest.mark.asyncio
async def test_import_graph_merge_confidence_bump_persists_across_restart(db_path):
    # Regression test: a merge-mode confidence bump on an existing edge was
    # memory-only and reverted to its pre-import value on the next restart.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    await ap.record_outcome("s1", "bump_me", 1.0, ctx)
    edge = _edge_by_primitive(ap, "bump_me")
    assert edge is not None
    edge.confidence = 0.1

    ok = await ap.import_graph({"edges": [{
        "id": edge.id,
        "confidence": 0.99,
    }]}, mode="merge")
    assert ok is True
    assert ap.get_edge(edge.id).confidence == 0.99

    await ap.session_close("s1")

    ap2 = AdaptivePathway(db_path=db_path)
    await ap2.session_open("s1")
    assert ap2.get_edge(edge.id).confidence == 0.99
    await ap2.session_close("s1")


@pytest.mark.asyncio
async def test_query_attribution(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    result = ap.query_attribution("nonexistent")
    assert result is None

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_health_check_returns_list(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    issues = ap.health_check()
    assert isinstance(issues, list)
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_run_maintenance(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(5):
        await ap.record_outcome("s1", f"action_{i}", 0.5, ctx)

    before = len(ap._action_history)
    await ap.run_maintenance()
    after = len(ap._action_history)
    assert before == after

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_run_maintenance_applies_confidence_decay(db_path):
    # decay.half_life_multiplier/base_half_life_hours existed in config but
    # were read nowhere in src/ — run_maintenance() now widens posterior
    # variance for buckets that have gone stale relative to their half-life.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    for _ in range(20):
        await ap.record_outcome("s1", "action_x", 1.0, ctx)

    bucket = ap._bucketer.get_bucket("action_x")
    ctx_features = np.asarray(ap._hasher.hash_embedding(ctx), dtype=np.float64)
    sigma_before = ap._ensemble.max_sigma(bucket, ctx_features)

    ap._ensemble.last_updated[bucket] -= 168 * 3600 * 10  # 10 half-lives stale
    await ap.run_maintenance()

    sigma_after = ap._ensemble.max_sigma(bucket, ctx_features)
    assert sigma_after > sigma_before

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_export_with_options(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    full_export = ap.export_graph(include_annotations=True, include_ensemble_state=True)
    assert "annotations" in full_export
    assert "ensemble_state" in full_export

    domain_export = ap.export_graph(domain="test_domain")
    assert "edges" in domain_export

    await ap.session_close("s1")


# ─── Schism resolution + ensemble weights ─────────────────


def test_get_schism_alert_none_by_default():
    ap = AdaptivePathway()
    assert ap.get_schism_alert() is None


def test_schism_detect_then_resolve():
    # `_detect()` only ever sets DETECTED (never REVIEWING, a real gap fixed by
    # relaxing ensemble.resolve()'s guard) — seed the same shape it produces.
    ap = AdaptivePathway()
    ap._ensemble.schism_state = SchismState.DETECTED
    ap._ensemble.schism_data = {"fa": [0, 1], "fb": [2, 3], "wa": 0.9, "wb": 0.85, "bt": 0.2}
    ap._ensemble.schism_detected_at = time.time()

    alert = ap.get_schism_alert()
    assert alert["state"] == "detected"
    assert alert["faction_a"] == [0, 1]
    assert alert["faction_b"] == [2, 3]
    assert alert["faction_a_models"] == 2
    assert alert["faction_b_models"] == 2
    assert alert["detected_at"] is not None

    assert ap.resolve_schism("a") is True
    assert ap._ensemble.schism_state == SchismState.RESOLVED
    # A second resolve attempt has nothing left to resolve.
    assert ap.resolve_schism("a") is False


def test_resolve_schism_keep_both():
    ap = AdaptivePathway()
    ap._ensemble.schism_state = SchismState.DETECTED
    ap._ensemble.schism_data = {"fa": [0, 1], "fb": [2, 3], "wa": 0.9, "wb": 0.85, "bt": 0.2}
    assert ap.resolve_schism("both") is True
    assert ap._ensemble.schism_state == SchismState.RESOLVED


def test_resolve_schism_no_active_schism():
    ap = AdaptivePathway()
    assert ap.resolve_schism("a") is False
    assert ap.resolve_schism("both") is False


@pytest.mark.asyncio
async def test_update_ensemble_weights_live_and_reflected_in_state():
    ap = AdaptivePathway()
    result = await ap.update_ensemble_weights(ig_weight_min=0.2, pc_weight=0.18)
    assert result["ig_weight_min"] == 0.2
    assert result["pc_weight"] == 0.18
    assert result["ig_weight_max"] == ap._ensemble.ig_weight_max  # untouched field passed through
    assert ap._ensemble.ig_weight_min == 0.2
    assert ap._ensemble.pc_weight == 0.18

    state = ap.get_state()
    assert state["ensemble_weights"]["ig_weight_min"] == 0.2
    assert state["ensemble_weights"]["pc_weight"] == 0.18


@pytest.mark.asyncio
async def test_exploration_metrics_in_get_metrics(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    metrics = ap.get_metrics()
    eh = metrics["metrics"].get("exploration_health")
    assert eh is not None
    assert "ig_pc_hint_ratio" in eh
    assert "action_entropy_50w" in eh
    assert "unique_primitives_active" in eh
    assert "wildcard_slot_used" in eh
    assert "user_exploration_score" in eh
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_session_reflection(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    reflection = ap.generate_session_reflection("s1")
    assert "reflection" in reflection
    assert "top_domains" in reflection
    assert "unchosen_novel_edges" in reflection
    assert "has_untested" in reflection
    assert "exploration_health" in reflection
    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_user_exploration_tracked(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    ap.decide("s1", ctx, ["action_a"])
    ap._novelty.record_user_action("action_never_hinted")
    assert ap._novelty.user_exploration_score > 0

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_nudge_offered_flag(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    result = ap.decide("s1", ctx, ["action_a", "action_b"])
    assert hasattr(result, "nudge_offered")

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_exploration_metrics_in_decide(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    result = ap.decide("s1", ctx, ["action_a"])
    assert result.exploration_metrics is not None
    assert "wildcard_count" in result.exploration_metrics
    assert "user_exploration_score" in result.exploration_metrics

    await ap.session_close("s1")


def test_action_entropy_empty():
    from collections import Counter
    ap = AdaptivePathway()
    ent = ap._compute_action_entropy(50)
    assert ent == 0.0


def test_action_entropy_with_history():
    ap = AdaptivePathway()
    ap._action_history = [f"action_{i % 5}" for i in range(60)]
    ent = ap._compute_action_entropy(50)
    assert 0.0 <= ent <= 1.01


def test_reward_weight_decay_no_edge_id():
    from adaptive_pathway.learning.preferences import PreferenceDetector, PreferenceIntensity
    config = {
        "preferences": {
            "centroid_min_examples": 50, "centroid_refresh_days": 30,
            "centroid_max_age_days": 60, "embedding_confidence_threshold": 0.7,
            "embedding_uncertainty_threshold": 0.3,
            "behavioral_confirmation_wait_turns": 1,
            "heuristic_fallback_confidence": 0.4, "intensity_mild": 0.3,
            "intensity_moderate": 0.7, "keep_this_weight_mild": 0.40,
            "keep_this_weight_moderate": 0.60, "keep_this_weight_strong": 0.80,
            "dont_do_again_weight_mild": -0.30,
            "dont_do_again_weight_moderate": -0.45,
            "dont_do_again_weight_strong": -0.60,
            "lambda_boost_session_only": True,
            "lambda_boost_plus_one_session": True,
            "lambda_boost_plus_two_sessions": True,
            "negative_pref_half_life_days": 0,
        }
    }
    detector = PreferenceDetector(config)
    rw = detector._reward_weight("dont_do_again", PreferenceIntensity.STRONG)
    assert rw == -0.60  # No decay when half-life is 0


def test_reward_weight_decay_with_edge():
    from adaptive_pathway.learning.preferences import PreferenceDetector, PreferenceIntensity
    config = {
        "preferences": {
            "centroid_min_examples": 50, "centroid_refresh_days": 30,
            "centroid_max_age_days": 60, "embedding_confidence_threshold": 0.7,
            "embedding_uncertainty_threshold": 0.3,
            "behavioral_confirmation_wait_turns": 1,
            "heuristic_fallback_confidence": 0.4, "intensity_mild": 0.3,
            "intensity_moderate": 0.7, "keep_this_weight_mild": 0.40,
            "keep_this_weight_moderate": 0.60, "keep_this_weight_strong": 0.80,
            "dont_do_again_weight_mild": -0.30,
            "dont_do_again_weight_moderate": -0.45,
            "dont_do_again_weight_strong": -0.60,
            "lambda_boost_session_only": True,
            "lambda_boost_plus_one_session": True,
            "lambda_boost_plus_two_sessions": True,
            "negative_pref_half_life_days": 45,
        }
    }
    detector = PreferenceDetector(config)
    detector._penalty_timestamps["edge_x"] = time.time() - 10  # 10 seconds old
    rw = detector._reward_weight("dont_do_again", PreferenceIntensity.STRONG, edge_id="edge_x")
    assert rw < -0.30  # Decay should reduce penalty somewhat; half-life check


# ─── Bug-fix regressions ───────────────────────────────────────────────


def test_health_checker_sees_real_edges():
    # HealthChecker was wired to a bucket-lookup function and always called
    # it with [] (engine.py's health_checker constructor), so get_graph_health()
    # always reported total_edges=0 regardless of the real graph.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway()
    bucket = ap._bucketer.get_bucket("some_primitive")
    ap._edge_index[bucket] = [EdgeInfo(id="e1", semantic_primitive="some_primitive", confidence=0.9)]
    health = ap.get_graph_health()
    assert health.total_edges == 1


@pytest.mark.asyncio
async def test_query_attribution_resolves_real_hint(db_path):
    # attribution_id used to be a bare uuid4 with no link back to the edge
    # that produced it — query_attribution() always returned None for a
    # real hint. decide() now logs attribution_id -> edge_id so it resolves.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("known_primitive")
    ap._edge_index[bucket] = [EdgeInfo(id="edge_known", semantic_primitive="known_primitive", confidence=0.9)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    # available_actions must match the edge's semantic_primitive (the
    # action-bucket lookup key), not its id.
    result = ap.decide("s1", ctx, ["known_primitive"])
    assert len(result.hints) >= 1
    hint = result.hints[0]

    attribution = ap.query_attribution(hint.attribution_id)
    assert attribution is not None
    assert attribution["edge_id"] == hint.edge_id

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_novelty_lambda_boost_set_and_decayed(db_path):
    # record_annotation's "dont_do_again" branch used to increment a
    # per-session field that nothing ever read. It's now engine-level state
    # consumed by the selector and decayed across session_close calls.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    assert ap._novelty_lambda_boost == 0.0
    await ap.record_annotation("s1", {
        "type": "dont_do_again",
        "edge_id": "edge_x",
        "context_embedding": ctx,
        "intensity": 0.9,  # >= intensity_moderate
    })
    assert ap._novelty_lambda_boost > 0.0
    sessions_remaining = ap._novelty_lambda_boost_sessions_remaining
    assert sessions_remaining > 0

    for _ in range(sessions_remaining):
        await ap.session_close("s1")
        await ap.session_open("s1")
    assert ap._novelty_lambda_boost == 0.0

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_session_isolation_last_hints_and_co_selected(db_path):
    # _last_hints/_co_selected/_wildcard_count used to be engine-global,
    # so concurrent sessions clobbered each other's exploration bookkeeping.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    await ap.session_open("s2")

    bucket = ap._bucketer.get_bucket("shared_primitive")
    ap._edge_index[bucket] = [EdgeInfo(id="e1", semantic_primitive="shared_primitive", confidence=0.9)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    ap.decide("s1", ctx, ["shared_primitive"])
    assert len(ap._sessions["s1"].last_hints) >= 1
    assert len(ap._sessions["s2"].last_hints) == 0

    await ap.session_close("s1")
    await ap.session_close("s2")


@pytest.mark.asyncio
async def test_domain_inference_wired_into_decide(db_path):
    # DomainDiscovery.infer_domain/update_domain_centroid existed but were
    # never called — domains never got centroids, so inference was always a
    # no-op. record_outcome now feeds centroids and decide() now infers a
    # domain when the session has no explicit domain_hint.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1", domain_hint="python")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    ap._domain_discovery.add_domain("python", "Python")
    await ap.record_outcome("s1", "some_action", 0.5, ctx)
    assert ap._domain_discovery.get_domain("python").get("centroid") is not None

    inferred = ap._domain_discovery.infer_domain(ctx, [], [])
    assert inferred == "python"

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_primitive_discovery_wired_into_record_outcome(db_path):
    # PrimitiveDiscoverer.maybe_discover existed but was never called from
    # the engine — record_outcome now feeds it, self-gated to every
    # primitive_call_interval calls.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path, **{"discovery.primitive_call_interval": 5})
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("recurring_action")
    ap._edge_index[bucket] = [EdgeInfo(id="recurring_action", semantic_primitive="recurring_action", confidence=0.9)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for i in range(5):
        await ap.record_outcome("s1", "recurring_action", 0.5, ctx)

    assert "recurring_action" in ap._primitive_discoverer.get_all_primitives()
    assert ap.get_state()["discovered_primitives_count"] == 1

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_import_graph_round_trips_embedding(db_path):
    # EdgeInfo.embedding was never populated anywhere (neither _warm_data
    # nor import_graph), which silently disabled DPP diversity selection
    # (it always fell back to all-zero vectors). export_graph now serializes
    # embeddings and import_graph restores them.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    emb = np.random.randn(384).astype(np.float32)
    ok = await ap.import_graph({
        "edges": [{
            "id": "edge_with_embedding",
            "semantic_primitive": "prim_x",
            "confidence": 0.8,
            "embedding": emb.tolist(),
        }]
    }, mode="merge")
    assert ok is True

    edge = ap.get_edge("edge_with_embedding")
    assert edge.embedding is not None
    assert np.allclose(edge.embedding, emb, atol=1e-5)

    exported = ap.export_graph()
    exported_edge = next(e for e in exported["edges"] if e["id"] == "edge_with_embedding")
    assert exported_edge["embedding"] is not None
    assert len(exported_edge["embedding"]) == 384

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_pc_score_labels_hint_source_model(db_path):
    # The paradigm-challenge score was computed per top edge and discarded —
    # every hint was hardcoded source_model="standard", so ig_pc_hint_ratio
    # / ig_pc_count could never see a real "pc" contribution. Force a high
    # PC score (the model's own scoring math is covered separately in
    # test_paradigm_challenge.py) and verify the selector actually uses it
    # to label the hint and annotate the rationale.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path, **{"paradigm_challenge.label_threshold": 0.5})
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("challenging_primitive")
    ap._edge_index[bucket] = [EdgeInfo(
        id="challenging_edge", semantic_primitive="challenging_primitive", confidence=0.9,
    )]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])
    ap._ensemble.models[ap._ensemble.pc_model_index].score_with_referents = lambda *a, **kw: 0.9

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    result = ap.decide("s1", ctx, ["challenging_primitive"])
    assert len(result.hints) >= 1
    assert result.hints[0].source_model == "pc"
    assert "challenges the current paradigm" in result.hints[0].rationale

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_uncertainty_slot_appears_with_surplus_edges(db_path):
    # Reserve-a-slot design: with more candidate edges than fit in the
    # standard top-K, one hint should always be the highest-epistemic-
    # uncertainty edge outside the preference-ranked top edges, labeled
    # source_model="uncertain" — independent of preference score.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    for i in range(8):
        bucket = ap._bucketer.get_bucket(f"prim_{i}")
        ap._edge_index[bucket] = [EdgeInfo(id=f"edge_{i}", semantic_primitive=f"prim_{i}", confidence=0.5)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    result = ap.decide("s1", ctx, [f"prim_{i}" for i in range(8)])

    uncertain_hints = [h for h in result.hints if h.source_model == "uncertain"]
    assert len(uncertain_hints) == 1
    assert "least data" in uncertain_hints[0].rationale
    assert result.exploration_metrics["uncertainty_slot_count"] == 1

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_uncertainty_slot_disabled_via_config(db_path):
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path, **{"exploration_slot.enabled": False})
    await ap.session_open("s1")

    for i in range(8):
        bucket = ap._bucketer.get_bucket(f"prim_{i}")
        ap._edge_index[bucket] = [EdgeInfo(id=f"edge_{i}", semantic_primitive=f"prim_{i}", confidence=0.5)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    result = ap.decide("s1", ctx, [f"prim_{i}" for i in range(8)])

    assert all(h.source_model != "uncertain" for h in result.hints)

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_uncertainty_slot_noop_on_small_graph(db_path):
    # With only 1 edge total there's no "remaining" pool to promote from —
    # the reservation logic must not shrink the (already tiny) hint list.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("only_primitive")
    ap._edge_index[bucket] = [EdgeInfo(id="only_edge", semantic_primitive="only_primitive", confidence=0.5)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    result = ap.decide("s1", ctx, ["only_primitive"])
    assert len(result.hints) >= 1
    assert result.hints[0].source_model in ("standard", "pc")

    await ap.session_close("s1")


def test_get_domain_resolves_edge_id_to_real_domain():
    # _get_domain is passed to ParadigmChallengeModel as get_domain_fn and
    # is always called with an *edge id*, not a domain id — it used to echo
    # the edge id straight back, so ParadigmChallengeModel.score()'s
    # domain-keyed lookups against domain_stats (keyed by real domain ids)
    # never matched, permanently zeroing the confidence_gap and
    # novelty_persistence signals.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway()
    bucket = ap._bucketer.get_bucket("prim")
    ap._edge_index[bucket] = [EdgeInfo(id="edge_1", semantic_primitive="prim", domain_id="python")]
    assert ap._get_domain("edge_1") == "python"
    assert ap._get_domain("unknown_edge_id") == "unknown_edge_id"  # fallback: identity
    assert ap._get_domain("") == ""


# ─── Failure-scenario fixes: misattribution, frequency bleed, topic lock-in ─


@pytest.mark.asyncio
async def test_micro_reward_caps_at_per_session_limit(db_path):
    # Scenario 3(a): a heavy research session generating many implicit
    # micro_positive signals (e.g. repeated Copy clicks) must saturate
    # rather than ratchet a topic's confidence without bound.
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for _ in range(50):
        await ap.record_annotation("s1", {
            "type": "micro_positive", "edge_id": "edge_x",
            "context_embedding": ctx, "intensity": 0.6,
        })

    cap = ap.config["telemetry"]["per_session_cap"]
    assert ap._sessions["s1"].micro_reward_used == pytest.approx(cap)

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_micro_reward_cap_is_per_session_not_global(db_path):
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")
    await ap.session_open("s2")

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    for _ in range(50):
        await ap.record_annotation("s1", {
            "type": "micro_positive", "edge_id": "edge_x",
            "context_embedding": ctx, "intensity": 0.6,
        })

    cap = ap.config["telemetry"]["per_session_cap"]
    assert ap._sessions["s1"].micro_reward_used == pytest.approx(cap)
    assert ap._sessions["s2"].micro_reward_used == 0.0

    await ap.session_close("s1")
    await ap.session_close("s2")


@pytest.mark.asyncio
async def test_dont_do_again_moderate_intensity_suppresses_edge_from_hints(db_path):
    # Scenario 3(c): topic-level "stop suggesting this" — a moderate+
    # rejection must remove the edge from decide() hints outright, not
    # just nudge its confidence down.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("sensitive_topic")
    ap._edge_index[bucket] = [EdgeInfo(id="sensitive_topic", semantic_primitive="sensitive_topic", confidence=0.9)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    before = ap.decide("s1", ctx, ["sensitive_topic"])
    assert len(before.hints) == 1

    await ap.record_annotation("s1", {
        "type": "dont_do_again", "edge_id": "sensitive_topic",
        "context_embedding": ctx, "intensity": 0.9,
    })

    entry = ap._ttl.get_ttl("sensitive_topic")
    assert entry is not None
    assert entry["cause"] == "user_rejected"

    after = ap.decide("s1", ctx, ["sensitive_topic"])
    assert len(after.hints) == 0

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_dont_do_again_mild_intensity_does_not_suppress_edge(db_path):
    # A mild rejection shouldn't trigger the month-long topic-level mute —
    # only a clearly deliberate, moderate+ signal should.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("some_topic")
    ap._edge_index[bucket] = [EdgeInfo(id="some_topic", semantic_primitive="some_topic", confidence=0.9)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)

    await ap.record_annotation("s1", {
        "type": "dont_do_again", "edge_id": "some_topic",
        "context_embedding": ctx, "intensity": 0.2,
    })

    assert ap._ttl.get_ttl("some_topic") is None

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_decide_with_real_context_differs_from_zero_context(db_path):
    # Scenario 2: without real embeddings, every call collapses to the same
    # constant context, so novelty/domain signals can't distinguish topics.
    from adaptive_pathway.types import EdgeInfo
    ap = AdaptivePathway(db_path=db_path)
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("some_action")
    ap._edge_index[bucket] = [EdgeInfo(id="some_action", semantic_primitive="some_action", confidence=0.5)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx_a = ap.embed_context("reviewing a novel draft about violence")
    ctx_b = ap.embed_context("writing a privacy policy for a service")
    assert not np.allclose(ctx_a, ctx_b)

    ap.decide("s1", ctx_a, ["some_action"])
    novelty_after_a = ap._novelty.current_score(ctx_a)
    novelty_b_before_use = ap._novelty.current_score(ctx_b)
    # A never-visited context should still read as fully novel even after a
    # different context was just visited — proof the two aren't colliding
    # into the same novelty bucket the way constant-zero contexts would.
    assert novelty_b_before_use == pytest.approx(1.0)
    assert novelty_after_a < 1.0

    await ap.session_close("s1")


def test_embed_context_hashing_fallback_is_available_without_ollama():
    ap = AdaptivePathway()
    ap._embedder._urlopen = lambda req, timeout=None: (_ for _ in ()).throw(OSError("no ollama"))
    v = ap.embed_context("some task context")
    assert v.shape == (ap.config["embedding_dim"],)
    assert not np.allclose(v, 0)


@pytest.mark.asyncio
async def test_update_ensemble_weights_rejects_invalid():
    ap = AdaptivePathway()
    result = await ap.update_ensemble_weights(ig_weight_min=1.5)
    assert "error" in result
    # Rejected update must not mutate live state.
    assert ap._ensemble.ig_weight_min != 1.5


@pytest.mark.asyncio
async def test_update_ensemble_weights_rejects_overallocation():
    ap = AdaptivePathway()
    result = await ap.update_ensemble_weights(ig_weight_max=0.8, pc_weight=0.5)
    assert "error" in result


def test_add_labeled_example_records_timestamp():
    from adaptive_pathway.learning.preferences import PreferenceDetector
    config = {
        "preferences": {
            "centroid_min_examples": 50, "centroid_refresh_days": 30,
            "centroid_max_age_days": 60, "embedding_confidence_threshold": 0.7,
            "embedding_uncertainty_threshold": 0.3,
            "behavioral_confirmation_wait_turns": 1,
            "heuristic_fallback_confidence": 0.4, "intensity_mild": 0.3,
            "intensity_moderate": 0.7, "keep_this_weight_mild": 0.40,
            "keep_this_weight_moderate": 0.60, "keep_this_weight_strong": 0.80,
            "dont_do_again_weight_mild": -0.30,
            "dont_do_again_weight_moderate": -0.45,
            "dont_do_again_weight_strong": -0.60,
            "lambda_boost_session_only": True,
            "lambda_boost_plus_one_session": True,
            "lambda_boost_plus_two_sessions": True,
            "negative_pref_half_life_days": 45,
        }
    }
    detector = PreferenceDetector(config)
    emb = np.random.randn(384).astype(np.float64)
    emb /= np.linalg.norm(emb)
    detector.add_labeled_example(emb, "dont_do_again", intensity=0.5, edge_id="bad_edge")
    assert "bad_edge" in detector._penalty_timestamps
    detector.add_labeled_example(emb, "keep_this", intensity=0.5, edge_id="good_edge")
    assert "good_edge" not in detector._penalty_timestamps
