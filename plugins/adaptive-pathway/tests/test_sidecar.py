import base64
import os
import tempfile
from unittest.mock import Mock

import numpy as np
import pytest
from fastapi.testclient import TestClient

from adaptive_pathway import AdaptivePathway
from adaptive_pathway.integrations.sidecar.server import (
    _quiet_connection_reset_handler,
    create_app,
)


def test_swallows_connection_reset_error():
    loop = Mock()
    context = {"exception": ConnectionResetError("[WinError 10054] ...")}
    _quiet_connection_reset_handler(loop, context)
    loop.default_exception_handler.assert_not_called()


def test_delegates_other_exceptions_to_default_handler():
    loop = Mock()
    context = {"exception": ValueError("something real broke")}
    _quiet_connection_reset_handler(loop, context)
    loop.default_exception_handler.assert_called_once_with(context)


def test_delegates_when_context_has_no_exception():
    loop = Mock()
    context = {"message": "some non-exception asyncio warning"}
    _quiet_connection_reset_handler(loop, context)
    loop.default_exception_handler.assert_called_once_with(context)


@pytest.fixture
def client():
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    ap = AdaptivePathway(db_path=path)
    app = create_app(ap)
    with TestClient(app) as c:
        yield c
    try:
        os.remove(path)
    except PermissionError:
        pass


def test_accept_nudge_requires_session_id(client):
    resp = client.post("/nudge/accept")
    assert resp.status_code == 422


def test_accept_nudge_activates_nudge_for_unopened_session(client):
    # No /session/open call first — accept_nudge should still work (falls
    # back to "thought_partner" mode) rather than 404/500, matching the MCP
    # tool's own `state.mode if state else "thought_partner"` behavior.
    resp = client.post("/nudge/accept", params={"session_id": "s1"})
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "accepted"
    assert body["active"] is True
    assert isinstance(body["multiplier"], (int, float))


def test_accept_nudge_uses_open_session_mode(client):
    open_resp = client.post("/session/open", json={"session_id": "s2", "mode": "agentic"})
    assert open_resp.status_code == 200
    resp = client.post("/nudge/accept", params={"session_id": "s2"})
    assert resp.status_code == 200
    assert resp.json()["status"] == "accepted"


def test_session_reflection_requires_session_id(client):
    resp = client.get("/session_reflection")
    assert resp.status_code == 422


def test_session_reflection_returns_expected_shape(client):
    client.post("/session/open", json={"session_id": "s3"})
    resp = client.get("/session_reflection", params={"session_id": "s3"})
    assert resp.status_code == 200
    body = resp.json()
    assert body["session_id"] == "s3"
    for key in (
        "top_domains",
        "acceptance_score",
        "unchosen_novel_edges",
        "reflection",
        "has_untested",
        "exploration_health",
    ):
        assert key in body


# ─── Hot-path auto-open (no /session/open call first) ─────────────────────


def test_decide_auto_opens_unopened_session(client):
    emb = np.zeros(384, dtype=np.float32).tobytes()
    b64 = base64.b64encode(emb).decode()
    resp = client.post("/decide", params={
        "session_id": "never_opened_decide",
        "context_embedding": b64,
    })
    assert resp.status_code == 200
    body = resp.json()
    assert "hints" in body
    assert "confidence" in body


def test_outcome_auto_opens_unopened_session(client):
    resp = client.post("/outcome", params={"session_id": "never_opened_outcome"}, json={
        "action_id": "action_a",
        "reward": 1.0,
        "context_embedding": [0.0] * 384,
    })
    assert resp.status_code == 200
    assert resp.json()["status"] == "recorded"


def test_annotation_auto_opens_unopened_session(client):
    resp = client.post("/annotation", params={"session_id": "never_opened_annotation"}, json={
        "type": "keep_this",
        "edge_id": "edge_a",
        "context_embedding": [0.0] * 384,
    })
    assert resp.status_code == 200
    assert resp.json()["status"] == "recorded"


# ─── Missing-body 400s (previously an unhandled 500 AttributeError) ───────


def test_update_edge_missing_body_returns_400(client):
    resp = client.put("/edges/some_edge")
    assert resp.status_code == 400


def test_update_domain_missing_body_returns_400(client):
    resp = client.put("/domains/some_domain")
    assert resp.status_code == 400


# ─── Ensemble weight validation ────────────────────────────────────────────


def test_update_ensemble_weights_rejects_out_of_range(client):
    resp = client.put("/config/ensemble", json={"ig_weight_min": 1.5})
    assert resp.status_code == 400


def test_update_ensemble_weights_rejects_overallocation(client):
    resp = client.put("/config/ensemble", json={"ig_weight_max": 0.8, "pc_weight": 0.5})
    assert resp.status_code == 400


# ─── Context param (frequency-bleed fix) ───────────────────────────────────


def test_decide_accepts_context_text_param(client):
    resp = client.post("/decide", params={
        "session_id": "ctx_decide", "context": "reviewing a novel draft",
    })
    assert resp.status_code == 200
    body = resp.json()
    assert "hints" in body


def test_outcome_accepts_context_text_param(client):
    resp = client.post("/outcome", params={"session_id": "ctx_outcome"}, json={
        "action_id": "action_a", "reward": 1.0, "context": "writing a privacy policy",
    })
    assert resp.status_code == 200
    assert resp.json()["status"] == "recorded"


def test_annotation_accepts_context_text_param(client):
    resp = client.post("/annotation", params={"session_id": "ctx_annotation"}, json={
        "type": "keep_this", "edge_id": "edge_a", "context": "writing a privacy policy",
    })
    assert resp.status_code == 200
    assert resp.json()["status"] == "recorded"


def test_decide_context_embedding_b64_wins_over_context_text():
    # If both are given, the pre-computed embedding must win outright — the
    # text path shouldn't override an explicitly supplied vector.
    import base64
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    ap = AdaptivePathway(db_path=path)
    seen = {}
    original_decide = ap.decide

    def spying_decide(session_id, context_embedding, available_actions):
        seen["ctx"] = np.asarray(context_embedding).copy()
        return original_decide(session_id, context_embedding, available_actions)

    ap.decide = spying_decide
    app = create_app(ap)
    with TestClient(app) as c:
        zeros_b64 = base64.b64encode(np.zeros(384, dtype=np.float32).tobytes()).decode()
        c.post("/decide", params={
            "session_id": "b64_wins", "context_embedding": zeros_b64,
            "context": "this text should be ignored",
        })
    assert np.allclose(seen["ctx"], 0)
    try:
        os.remove(path)
    except PermissionError:
        pass


# ─── Maintenance loop (topic lock-in / stale-confidence fix) ──────────────


def test_maintenance_loop_fires_once_engine_warms():
    # Nothing previously called run_maintenance() in production — the
    # confidence-decay half-life and cold-edge pruning it drives were fully
    # built but dormant. Confirms the sidecar now self-schedules it rather
    # than depending on a client to ever call POST /maintenance.
    import time as time_module

    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    ap = AdaptivePathway(db_path=path, **{
        "maintenance.startup_poll_s": 0.02,
        "maintenance.interval_hours": 0.0001,  # floored to 60s for the repeat; only the first run matters here
    })

    calls = {"n": 0}
    original_run_maintenance = ap.run_maintenance

    async def counting_run_maintenance():
        calls["n"] += 1
        await original_run_maintenance()

    ap.run_maintenance = counting_run_maintenance
    app = create_app(ap)
    with TestClient(app) as c:
        # Opening a session sets ap._engine, which the background loop is
        # polling for (at the tiny poll interval configured above).
        c.post("/session/open", json={"session_id": "warmup"})
        for _ in range(50):
            if calls["n"] > 0:
                break
            time_module.sleep(0.05)
    assert calls["n"] >= 1
    try:
        os.remove(path)
    except PermissionError:
        pass


def test_update_ensemble_weights_accepts_valid_values(client):
    resp = client.put("/config/ensemble", json={"ig_weight_min": 0.2, "pc_weight": 0.1})
    assert resp.status_code == 200
    body = resp.json()
    assert body["ig_weight_min"] == 0.2
    assert body["pc_weight"] == 0.1


# ─── /state embedding block (Kitty verifies real vectors are in use) ──────


def test_state_includes_embedding_block(client):
    resp = client.get("/state")
    assert resp.status_code == 200
    body = resp.json()
    assert "embedding" in body
    assert set(body["embedding"].keys()) == {"backend", "model", "url", "failed_decodes"}
    assert body["embedding"]["backend"] == "untried"
    assert body["embedding"]["failed_decodes"] == 0


# ─── /decide full payload + available_actions (single-engine architecture) ─


def test_decide_returns_full_payload(client):
    resp = client.post("/decide", params={"session_id": "full_payload"})
    assert resp.status_code == 200
    body = resp.json()
    assert "hints" in body
    assert "confidence" in body
    assert "novelty" in body
    assert "attribution_ids" in body
    assert "is_flow_state" in body
    assert "nudge_offered" in body


def test_decide_passes_available_actions_to_engine(client):
    import base64

    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    ap = AdaptivePathway(db_path=path)
    seen = {}
    original_decide = ap.decide

    def spying_decide(session_id, context_embedding, available_actions):
        seen["actions"] = list(available_actions)
        return original_decide(session_id, context_embedding, available_actions)

    ap.decide = spying_decide
    app = create_app(ap)
    with TestClient(app) as c:
        c.post("/decide", params={
            "session_id": "avail_actions",
            "available_actions": "edit, shell, write",
        })
    assert seen["actions"] == ["edit", "shell", "write"]
    try:
        os.remove(path)
    except PermissionError:
        pass


def test_outcome_error_type_crash_pins_ttl(client):
    # Row 7 of 82inefficiencies.md: a negative reward must NOT auto-set a
    # syntax_crash TTL — only an explicit error_type="crash" signal may.
    resp = client.post("/outcome", params={"session_id": "ttl_crash"}, json={
        "action_id": "fragile_action",
        "reward": -1.0,
        "error_type": "crash",
        "context_embedding": [0.0] * 384,
    })
    assert resp.status_code == 200
    assert resp.json()["status"] == "recorded"

    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    ap = AdaptivePathway(db_path=path)
    app = create_app(ap)
    with TestClient(app) as c:
        c.post("/session/open", json={"session_id": "s"})
        c.post("/outcome", params={"session_id": "s"}, json={
            "action_id": "plain_failure",
            "reward": -0.8,
            "context_embedding": [0.0] * 384,
        })
        c.post("/outcome", params={"session_id": "s"}, json={
            "action_id": "crash_failure",
            "reward": -0.8,
            "error_type": "crash",
            "context_embedding": [0.0] * 384,
        })
    assert ap._ttl.is_expired("plain_failure") is False
    assert ap._ttl.is_expired("crash_failure") is True
    try:
        os.remove(path)
    except PermissionError:
        pass


def test_malformed_b64_embedding_counts_failure(client):
    resp = client.post("/decide", params={
        "session_id": "bad_b64",
        "context_embedding": "not-valid-base64!!!",
    })
    assert resp.status_code == 200
    state = client.get("/state")
    assert state.json()["embedding"]["failed_decodes"] == 1
