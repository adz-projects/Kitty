"""Contract tests against Kitty (../Kitty), the Tauri desktop app that
consumes this package as an MCP stdio server + FastAPI sidecar.

These pin the exact response shapes Kitty's frontend expects, sourced from:
- Kitty/src/lib/pyrepr.ts        — Python-repr parser for the MCP `decide` tool's text output
- Kitty/src/stores/chatStore.ts  — AdaptivePathwayHint / ParsedHintOutput shapes, parseHintOutput()
- Kitty/src-tauri/src/adaptive_pathway/mod.rs — sidecar REST endpoints Kitty calls

They do not require Kitty itself to be present/running — they assert
against fixed contracts transcribed from that source, so they'll fail loudly
if this package's response shapes drift without a corresponding Kitty update.
"""
import ast
import os
import tempfile

import numpy as np
import pytest
from fastapi.testclient import TestClient

from adaptive_pathway import AdaptivePathway
from adaptive_pathway.mcp_server import _format_result
from adaptive_pathway.integrations.sidecar.server import create_app


def _parse_pyrepr(text: str):
    """Python-side proxy for Kitty's pyrepr.ts: str(dict) output must be
    parseable as a Python literal (same grammar pyrepr.ts implements:
    quoted strings, True/False/None, {}/[]/())."""
    return ast.literal_eval(text)


@pytest.fixture
def ap_with_edge():
    fd, path = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    ap = AdaptivePathway(db_path=path)
    yield ap
    try:
        os.remove(path)
    except PermissionError:
        pass


# ─── MCP `decide` tool output — parsed by Kitty's pyrepr.ts / chatStore.ts ─


@pytest.mark.asyncio
async def test_decide_output_is_valid_pyrepr_with_required_top_level_keys(ap_with_edge):
    from adaptive_pathway.types import EdgeInfo
    ap = ap_with_edge
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("prim_a")
    ap._edge_index[bucket] = [EdgeInfo(id="edge_a", semantic_primitive="prim_a", confidence=0.8)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    result = ap.decide("s1", ctx, ["prim_a"])
    text = str(_format_result(result))

    parsed = _parse_pyrepr(text)
    # chatStore.ts's parseHintOutput() requires these exact top-level keys.
    assert isinstance(parsed["hints"], list)
    assert isinstance(parsed["confidence"], (int, float))
    assert isinstance(parsed["novelty"], (int, float))
    assert isinstance(parsed["nudge_offered"], bool)

    await ap.session_close("s1")


@pytest.mark.asyncio
async def test_decide_hint_shape_matches_adaptive_pathway_hint_interface(ap_with_edge):
    # Kitty's AdaptivePathwayHint interface requires `text` (its filter
    # drops any hint object missing a string `text`) and reads
    # `confidence`, `type`, `edge_id`, `rationale`, `source_model`.
    from adaptive_pathway.types import EdgeInfo
    ap = ap_with_edge
    await ap.session_open("s1")

    bucket = ap._bucketer.get_bucket("prim_b")
    ap._edge_index[bucket] = [EdgeInfo(id="edge_b", semantic_primitive="prim_b", confidence=0.75, frequency=12)]
    ap._tiered.warm_from_db(ap._get_all_edges(), [])

    ctx = np.random.randn(384).astype(np.float32)
    ctx /= np.linalg.norm(ctx)
    result = ap.decide("s1", ctx, ["prim_b"])
    text = str(_format_result(result))
    parsed = _parse_pyrepr(text)

    assert len(parsed["hints"]) >= 1
    hint = parsed["hints"][0]
    assert isinstance(hint["text"], str)
    assert isinstance(hint["confidence"], (int, float))
    assert isinstance(hint["type"], str)
    assert "edge_id" in hint
    assert "rationale" in hint
    assert "source_model" in hint

    await ap.session_close("s1")


def test_format_result_handles_apostrophes_in_hint_text():
    # pyrepr.ts exists specifically because a naive quote-swap breaks on
    # hint text containing an apostrophe — verify str(dict) output for such
    # text still round-trips through ast.literal_eval (same grammar class
    # pyrepr.ts implements).
    from adaptive_pathway.types import Hint, DecisionResult
    hint = Hint(text="don't do this again — it's risky", confidence=0.5,
               primitive="p", domain="d", attribution_id="a1", edge_id="e1",
               rationale="don't repeat", source_model="standard")
    result = DecisionResult(hints=[hint], confidence=0.5, novelty=0.5,
                            attribution_ids=["a1"], is_flow_state=False,
                            nudge_offered=False, exploration_metrics={})
    text = str(_format_result(result))
    parsed = _parse_pyrepr(text)
    assert parsed["hints"][0]["text"] == "don't do this again — it's risky"


# ─── Sidecar REST — consumed by src-tauri/src/adaptive_pathway/mod.rs ─────


@pytest.fixture
def sidecar_client():
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


def test_state_includes_ensemble_weights_shape(sidecar_client):
    resp = sidecar_client.get("/state")
    assert resp.status_code == 200
    body = resp.json()
    assert set(body["ensemble_weights"].keys()) == {"ig_weight_min", "ig_weight_max", "pc_weight"}


def test_schism_none_shape(sidecar_client):
    resp = sidecar_client.get("/schism")
    assert resp.status_code == 200
    assert resp.json() == {"state": "none"}


def test_nudge_accept_shape(sidecar_client):
    resp = sidecar_client.post("/nudge/accept", params={"session_id": "s1"})
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "accepted"
    assert isinstance(body["active"], bool)
    assert isinstance(body["multiplier"], (int, float))


def test_session_reflection_shape(sidecar_client):
    sidecar_client.post("/session/open", json={"session_id": "s1"})
    resp = sidecar_client.get("/session_reflection", params={"session_id": "s1"})
    assert resp.status_code == 200
    body = resp.json()
    for key in ("session_id", "top_domains", "acceptance_score",
                "unchosen_novel_edges", "reflection", "has_untested", "exploration_health"):
        assert key in body


def test_get_edge_shape(sidecar_client):
    sidecar_client.post("/session/open", json={"session_id": "s1"})
    resp = sidecar_client.get("/edges/nonexistent")
    assert resp.status_code == 404  # "why suggested" link target contract: 404 when absent


def test_update_ensemble_config_shape(sidecar_client):
    resp = sidecar_client.put("/config/ensemble", json={"ig_weight_min": 0.25})
    assert resp.status_code == 200
    body = resp.json()
    assert set(body.keys()) == {"ig_weight_min", "ig_weight_max", "pc_weight"}
    assert body["ig_weight_min"] == 0.25
