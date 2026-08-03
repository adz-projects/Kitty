import argparse
import asyncio
import json
import os
from typing import Annotated
from mcp.server.fastmcp import FastMCP
from mcp.types import ToolAnnotations
from pydantic import Field

# ── MCP server ──────────────────────────────────────────────────────────
mcp = FastMCP("adaptive-pathway")

# ── HTTP proxy to the sidecar ───────────────────────────────────────────
# The sidecar process owns the single AdaptivePathway engine (and its
# SQLite DB). This stdio MCP server is a thin stateless proxy: every tool
# maps to one REST call. There is deliberately NO embedded engine here —
# two processes writing the same DB last-writer-wins (the old split-brain
# design) is what made TTL state, ensemble weights, and session bookkeeping
# silently diverge between the two. `AP_SIDECAR_PORT` is set by Kitty when
# it registers this server with BigTiny; defaults to the sidecar's own port.
_SIDECAR_PORT = int(os.environ.get("AP_SIDECAR_PORT", "8700"))
_SIDECAR_BASE = f"http://127.0.0.1:{_SIDECAR_PORT}"
_HTTP_TIMEOUT = 10


def _http(method, path, params=None, body=None, timeout=_HTTP_TIMEOUT) -> tuple[int | None, dict]:
    import urllib.parse as up
    import urllib.request as ur

    url = _SIDECAR_BASE + path
    if params:
        url += "?" + up.urlencode(params, doseq=True)
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = ur.Request(url, data=data, headers=headers, method=method)
    try:
        with ur.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            try:
                return resp.status, json.loads(raw)
            except Exception:
                return resp.status, {"raw": raw}
    except ur.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, {"raw": raw}
    except Exception as e:
        return None, {"error": f"adaptive-pathway sidecar unreachable on {_SIDECAR_BASE}: {e}"}


async def _call(method, path, params=None, body=None) -> tuple[int | None, dict]:
    """Sidecar call off the event loop (blocking urllib; localhost, sub-ms)."""
    return await asyncio.to_thread(_http, method, path, params, body)


def _error_json(data, status):
    if isinstance(data, dict) and data.get("detail"):
        return json.dumps({"error": data["detail"]})
    if isinstance(data, dict) and data.get("error"):
        return json.dumps({"error": data["error"]})
    return json.dumps({"error": f"sidecar error (HTTP {status})"})


# ── decide result rendering ──────────────────────────────────────────────

def _decision_result_from_payload(payload):
    """Rebuild a DecisionResult from the sidecar's /decide payload so the
    single formatting function (`_format_result`) keeps owning the pyrepr
    shape Kitty parses. The sidecar returns raw structured data; it never
    renders client-facing text."""
    from adaptive_pathway.types import (
        BlendedHint, DecisionResult, Hint, InSessionStatus, NudgeStatus, PlateauRisk)

    hints = []
    for h in payload.get("hints", []):
        if h.get("type") == "blended":
            sources = h.get("sources") or ["", ""]
            hints.append(BlendedHint(
                text=h.get("text", ""),
                confidence=h.get("confidence", 0.0),
                source_primitive_a=sources[0],
                source_primitive_b=sources[1] if len(sources) > 1 else sources[0],
                attribution_id=h.get("attribution_id", ""),
                edge_id=h.get("edge_id"),
            ))
        else:
            hints.append(Hint(
                text=h.get("text", ""),
                confidence=h.get("confidence", 0.0),
                primitive=h.get("primitive", ""),
                domain=h.get("domain", ""),
                attribution_id=h.get("attribution_id", ""),
                edge_id=h.get("edge_id"),
                rationale=h.get("rationale"),
                source_model=h.get("source_model", "standard"),
            ))

    plateau = payload.get("plateau_risk")
    if plateau:
        plateau = PlateauRisk(
            score=plateau.get("score", 0.0),
            entropy_risk=plateau.get("entropy_risk", 0.0),
            diversity_risk=plateau.get("diversity_risk", 0.0),
            novelty_risk=plateau.get("novelty_risk", 0.0),
            agreement_risk=plateau.get("agreement_risk", 0.0),
            trend=plateau.get("trend", "stable"),
            ig_weight=plateau.get("ig_weight", 0.0),
        )
    in_session = payload.get("in_session")
    if in_session:
        in_session = InSessionStatus(
            mix_weight=in_session.get("mix_weight", 0.0),
            call_count=in_session.get("call_count", 0),
            max_weight=in_session.get("max_weight", 0.0),
            buffer_size=in_session.get("buffer_size", 0),
        )
    nudge_active = payload.get("nudge_active")
    if nudge_active:
        nudge_active = NudgeStatus(
            active=nudge_active.get("active", True),
            multiplier=nudge_active.get("multiplier", 1.0),
            reason=nudge_active.get("reason", ""),
            turns_remaining=nudge_active.get("turns_remaining", 0),
        )

    return DecisionResult(
        hints=hints,
        confidence=payload.get("confidence", 0.0),
        novelty=payload.get("novelty", 0.0),
        attribution_ids=payload.get("attribution_ids", []),
        is_flow_state=payload.get("is_flow_state", False),
        plateau_risk=plateau,
        in_session=in_session,
        nudge_active=nudge_active,
        nudge_offered=payload.get("nudge_offered", False),
        exploration_metrics=payload.get("exploration_metrics"),
    )


def _format_result(decision_result):
    hints = []
    for h in decision_result.hints:
        if hasattr(h, "source_primitive_a"):
            hints.append({
                "text": h.text,
                "confidence": h.confidence,
                "type": "blended",
                "sources": [h.source_primitive_a, h.source_primitive_b],
                "attribution_id": h.attribution_id,
                "edge_id": h.edge_id,
                "rationale": getattr(h, "rationale", None),
                "source_model": getattr(h, "source_model", "standard"),
            })
        else:
            hints.append({
                "text": h.text,
                "confidence": h.confidence,
                "type": "single",
                "primitive": getattr(h, "primitive", ""),
                "domain": getattr(h, "domain", ""),
                "attribution_id": h.attribution_id,
                "edge_id": h.edge_id,
                "rationale": getattr(h, "rationale", None),
                "source_model": getattr(h, "source_model", "standard"),
            })
    out = {
        "hints": hints,
        "confidence": decision_result.confidence,
        "novelty": decision_result.novelty,
        "is_flow_state": decision_result.is_flow_state,
        "nudge_offered": decision_result.nudge_offered,
    }
    if decision_result.exploration_metrics:
        out["exploration_metrics"] = decision_result.exploration_metrics
    if decision_result.plateau_risk:
        out["plateau_risk"] = {
            "score": decision_result.plateau_risk.score,
            "ig_weight": decision_result.plateau_risk.ig_weight,
            "trend": decision_result.plateau_risk.trend,
        }
    if decision_result.in_session:
        out["in_session"] = {
            "mix_weight": decision_result.in_session.mix_weight,
            "call_count": decision_result.in_session.call_count,
        }
    if decision_result.nudge_active:
        out["nudge_active"] = True
    return out


# ── Tools ───────────────────────────────────────────────────────────────
# Parameter docs live in `Field(description=...)`, not the docstring: this
# `mcp` version dumps the whole docstring verbatim into the tool-level
# `description` (it does not parse an `Args:` section into per-parameter
# schema fields), so a long docstring both duplicates the Field text AND
# bloats the one blob that's actually sent — the entire schema below gets
# injected into the system prompt on every turn, hurting first-token
# latency and cache reuse, especially for small local models. Keep tool
# docstrings to one short sentence, and never restate a default in prose —
# the JSON schema's own `"default"` key already carries that.
SID = Annotated[str, Field(description="Session id")]

# Standard MCP tool-permission hints (mcp.types.ToolAnnotations) — the
# spec-correct way to tell an ACP/MCP client a tool is safe, so its
# approval-mode heuristic (Goose's `smart_approve`, in Kitty's case) doesn't
# need to prompt for explicit user approval before every call. None of these
# tools touch the filesystem, run shell commands, or reach the network
# beyond adaptive-pathway's own local sidecar/SQLite file.
_READ_ONLY = ToolAnnotations(
    readOnlyHint=True, destructiveHint=False, idempotentHint=False, openWorldHint=False
)
_WRITES_LOCAL_STATE = ToolAnnotations(
    readOnlyHint=False, destructiveHint=False, idempotentHint=False, openWorldHint=False
)


@mcp.tool(annotations=_READ_ONLY)
async def decide(
    session_id: SID = "default",
    available_actions: Annotated[str, Field(
        description="Comma-separated tool names you are choosing between, "
                     "e.g. 'edit,shell,write' or approach labels like "
                     "'style:short,style:detailed'")] = "",
    context: Annotated[str, Field(
        description="REQUIRED. Short summary of what this task is about, "
                     "e.g. 'editing a spreadsheet'. Prevents preferences "
                     "from bleeding across unrelated topics")] = "",
    context_embedding_b64: Annotated[str, Field(
        description="Pre-computed base64 float32 embedding (384 dims)")] = "",
) -> str:
    """MANDATORY before choosing a tool or drafting a substantive response. Returns learned tool/approach preferences."""
    params = {"session_id": session_id}
    if available_actions:
        params["available_actions"] = available_actions
    if context:
        params["context"] = context
    if context_embedding_b64:
        params["context_embedding"] = context_embedding_b64
    status, data = await _call("POST", "/decide", params=params)
    if status != 200:
        return _error_json(data, status)
    return str(_format_result(_decision_result_from_payload(data)))


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def record_outcome(
    session_id: SID = "default",
    action_id: Annotated[str, Field(
        description="REQUIRED. Name of the tool or approach label that was just used")] = "",
    reward: Annotated[float, Field(
        description="REQUIRED. 1.0 = success, -1.0 = failure, 0.0 = neutral")] = 0.0,
    context: Annotated[str, Field(
        description="REQUIRED. Same task summary you passed to decide")] = "",
    context_embedding_b64: Annotated[str, Field(
        description="Pre-computed embedding, wins over context if both given")] = "",
    is_blended: Annotated[bool, Field(
        description="True if multi-source action")] = False,
    blend_edge_ids: Annotated[str, Field(
        description="Comma-separated edge ids of the sources. REQUIRED (>=2) "
                     "when is_blended=true")] = "",
    error_type: Annotated[str, Field(
        description="Failure classification. 'crash' pins a crash TTL on the "
                     "action; leave empty for ordinary outcomes")] = "",
) -> str:
    """MANDATORY immediately after any tool call or substantive response. Records how it went."""
    if is_blended:
        ids = [i.strip() for i in blend_edge_ids.split(",") if i.strip()]
        if len(ids) < 2:
            return json.dumps({"error": "blend_edge_ids (>=2 comma-separated edge ids) is required when is_blended=true"})
    else:
        ids = []
    body = {"action_id": action_id, "reward": reward, "is_blended": is_blended}
    if ids:
        body["blend_edge_ids"] = ids
    if context:
        body["context"] = context
    if context_embedding_b64:
        body["context_embedding_b64"] = context_embedding_b64
    if error_type:
        body["error_type"] = error_type
    status, data = await _call("POST", "/outcome", params={"session_id": session_id}, body=body)
    if status != 200:
        return _error_json(data, status)
    return '{"status": "recorded"}'


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def record_annotation(
    session_id: SID = "default",
    annotation_type: Annotated[str, Field(
        description="keep_this | dont_do_again | micro_positive | "
                     "micro_negative | explore_alternative | retry_same_intent")] = "",
    action_id: Annotated[str, Field(
        description="The action/suggestion this feedback applies to")] = "",
    intensity: Annotated[float, Field(
        description="Signal strength 0.0-1.0")] = 0.5,
    context: Annotated[str, Field(
        description="Task summary (same as passed to decide)")] = "",
    context_embedding_b64: Annotated[str, Field(
        description="Pre-computed embedding, wins over context if both given")] = "",
) -> str:
    """Record explicit user feedback (e.g. 'keep this' / 'don't do that')."""
    body = {
        "type": annotation_type,
        "edge_id": action_id,
        "action_id": action_id,
        "intensity": intensity,
    }
    if context_embedding_b64:
        body["context_embedding_b64"] = context_embedding_b64
    elif context:
        body["context"] = context
    status, data = await _call("POST", "/annotation", params={"session_id": session_id}, body=body)
    if status != 200:
        return _error_json(data, status)
    return '{"status": "recorded"}'


@mcp.tool(annotations=_READ_ONLY)
async def get_state(session_id: SID = "default") -> str:
    """Full system snapshot: preferences, graph health, ensemble status, novelty health, domain profiles."""
    status, data = await _call("GET", "/state")
    if status != 200:
        return _error_json(data, status)
    return str(data)


@mcp.tool(annotations=_READ_ONLY)
async def list_edges(
    domain: Annotated[str, Field(description="Filter by domain")] = "",
    confidence_min: Annotated[float, Field(description="Minimum confidence threshold")] = 0.0,
    tier: Annotated[str, Field(description="Filter by tier: hot, warm, cold")] = "",
    page: Annotated[int, Field(description="Page number")] = 1,
    per_page: Annotated[int, Field(description="Results per page")] = 20,
) -> str:
    """Browse learned edges in the adaptive graph."""
    params: dict[str, str | int | float] = {"page": page, "per_page": per_page}
    if domain:
        params["domain"] = domain
    if confidence_min:
        params["confidence_min"] = confidence_min
    if tier:
        params["tier"] = tier
    status, data = await _call("GET", "/edges", params=params)
    if status != 200:
        return _error_json(data, status)
    return str(data)


@mcp.tool(annotations=_READ_ONLY)
async def get_edge(
    edge_id: Annotated[str, Field(description="ID of the edge to inspect")] = "",
) -> str:
    """Inspect a specific learned edge in detail."""
    if not edge_id:
        return '{"error": "edge_id required"}'
    import urllib.parse as up
    status, data = await _call("GET", f"/edges/{up.quote(edge_id, safe='')}")
    if status == 404:
        return '{"error": "edge not found"}'
    if status != 200:
        return _error_json(data, status)
    return str(data)


@mcp.tool(annotations=_READ_ONLY)
async def query_attribution(
    attribution_id: Annotated[str, Field(description="ID of the hint/attribution to explain")] = "",
) -> str:
    """Explain why a hint was suggested. Returns ensemble breakdown, IG score, PC signals, alternatives considered."""
    if not attribution_id:
        return '{"error": "attribution_id required"}'
    import urllib.parse as up
    status, data = await _call("GET", f"/attribution/{up.quote(attribution_id, safe='')}")
    if status == 404:
        return '{"error": "attribution not found"}'
    if status != 200:
        return _error_json(data, status)
    result = {
        "edge_id": data.get("edge_id"),
        "primitive": data.get("semantic_primitive"),
        "ensemble_mean": data.get("ensemble_mean", 0.0),
        "ensemble_std": data.get("ensemble_std", 0.0),
        "ensemble_agree": not data.get("ensemble_disagree", True),
        "ig_model_score": data.get("ig_model_score", 0.0),
        "pc_model_score": data.get("pc_model_score", 0.0),
        "alternatives": data.get("alternatives_considered", []),
    }
    return str(result)


@mcp.tool(annotations=_READ_ONLY)
async def list_domains() -> str:
    """List all learned domains with DPP diversity weight, novelty lambda, override rate, etc."""
    status, data = await _call("GET", "/domains")
    if status != 200:
        return _error_json(data, status)
    return str(data)


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def toggle_suggestions(
    session_id: SID = "default",
    paused: Annotated[bool, Field(description="True = pause, false = resume")] = True,
) -> str:
    """Pause or resume adaptive suggestions. Learning continues at reduced weight during pause."""
    status, data = await _call(
        "POST", "/suggestions/toggle",
        params={"session_id": session_id, "paused": str(paused).lower()},
    )
    if status != 200:
        return _error_json(data, status)
    if data.get("status") == "session_not_found":
        return '{"status": "error", "error": "session not found"}'
    return f'{{"status": "ok", "paused": {str(paused).lower()}}}'


@mcp.tool(annotations=_READ_ONLY)
async def health_check() -> str:
    """Run system diagnostics. Returns health across features, novelty, ensemble, graph, preferences, tier distribution."""
    status, data = await _call("GET", "/health")
    if status != 200:
        return _error_json(data, status)
    issues = data.get("issues", []) if isinstance(data, dict) else data
    return str(issues)


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def accept_nudge(session_id: SID = "default") -> str:
    """Accept an exploration nudge to boost alternative approaches for the next few turns."""
    status, data = await _call("POST", "/nudge/accept", params={"session_id": session_id})
    if status != 200:
        return _error_json(data, status)
    return json.dumps(data)


@mcp.tool(annotations=_READ_ONLY)
async def session_reflection(session_id: SID = "default") -> str:
    """Session summary: acceptance metrics, top domains, untested approaches."""
    status, data = await _call("GET", "/session_reflection", params={"session_id": session_id})
    if status != 200:
        return _error_json(data, status)
    return str(data)


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def resolve_schism(
    keep_faction: Annotated[str, Field(description="a | b | both")] = "both",
) -> str:
    """Resolve an ensemble schism. 'a' keeps faction A, 'b' keeps B, 'both' preserves both with widened variance."""
    status, data = await _call("POST", "/schism/resolve", params={"keep_faction": keep_faction})
    if status != 200:
        return _error_json(data, status)
    if data.get("status") == "resolved":
        return f'{{"status": "resolved", "faction": "{keep_faction}"}}'
    return '{"error": "no active schism"}'


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def session_close(session_id: SID = "default") -> str:
    """Call when the session ends: flushes co-selection learning, expires in-session lambda boosts, clears per-session state."""
    status, data = await _call("POST", "/session/close", params={"session_id": session_id})
    if status != 200:
        return _error_json(data, status)
    return '{"status": "closed"}'


# ── Prompts ─────────────────────────────────────────────────────────────
@mcp.prompt()
def adaptive_instructions() -> str:
    """Instructions for using the Adaptive Pathway extension.
    Call this prompt at the start of every session."""
    return """You have access to the Adaptive Pathway learning system.
It continuously learns which tools and approaches work best.

BEFORE selecting any tool or action:
1. Call `decide` with the list of tools you're choosing among.
   This returns learned preferences from past sessions.
2. Incorporate the returned hints into your decision.
   Hints show a rationale explaining WHY a tool is preferred.

AFTER every tool execution:
1. Call `record_outcome` with:
   - action_id: the tool you just used
   - reward: 1.0 for success, -1.0 for failure/error
   This builds the learning model. The more consistent you are,
   the better the suggestions become over time.

WILDCARD HINTS:
- Some hints are marked "untested angle" — these are experimental
  suggestions from domains or approaches the system has rarely explored.
  When you accept a wildcard hint and succeed, the system learns to
  surface more diverse suggestions for you.

CONSENT-BASED EXPLORATION:
- When the system detects you're circling the same few approaches,
  it may offer an exploration boost. If you see a nudge_offered flag,
  you can call `accept_nudge` to mix in alternative approaches for a
  few turns. You can decline with no penalty.

When the user gives explicit feedback ("keep this", "don't do that"):
- Call `record_annotation` with the appropriate annotation_type.
- Negative preferences (dont_do_again) naturally decay over time,
  so the system can re-explore rejected approaches after ~45 days.

You can also:
- Call `get_state` to see system health and learning progress.
- Call `session_reflection` for a summary of what's been learned.
- Call `toggle_suggestions paused=true` to pause hints temporarily.
- Call `resolve_schism keep_faction="both"` if models have diverged.
- Call `session_close` at the end of the session so its learnings
  are flushed and state is cleaned up."""


# ── Entry point ─────────────────────────────────────────────────────────
def main():
    parser = argparse.ArgumentParser(description="Adaptive Pathway MCP Server")
    parser.add_argument(
        "--db-path",
        default=os.environ.get("ADAPTIVE_PATHWAY_DB", "./pathway.db"),
        help="Path to the SQLite database (kept for backwards compatibility; "
             "the sidecar owns the database in the current architecture)",
    )
    parser.parse_args()
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
