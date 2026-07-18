import argparse
import asyncio
import sys
import os
from typing import Annotated
import numpy as np
from mcp.server.fastmcp import FastMCP, Context
from mcp.types import ToolAnnotations
from pydantic import Field

# ── MCP server ──────────────────────────────────────────────────────────
mcp = FastMCP("adaptive-pathway")

# ── Lazy engine init ────────────────────────────────────────────────────
_ap = None
_sessions = {}  # session_id -> opened state

def _get_ap(db_path=None):
    global _ap
    if _ap is None:
        from adaptive_pathway import AdaptivePathway
        path = db_path or os.environ.get("ADAPTIVE_PATHWAY_DB", "./pathway.db")
        _ap = AdaptivePathway(db_path=path)
    return _ap


async def _ensure_session(session_id=None, mode="thought_partner"):
    global _sessions
    ap = _get_ap()
    sid = session_id or "default"
    if sid not in _sessions:
        await ap.session_open(sid, mode=mode)
        _sessions[sid] = True
    return sid


# ── Helper ──────────────────────────────────────────────────────────────
def _decode_embedding(b64_string=""):
    if not b64_string:
        return np.zeros(384, dtype=np.float32)
    try:
        import base64
        raw = base64.b64decode(b64_string)
        return np.frombuffer(raw, dtype=np.float32)
    except Exception:
        return np.zeros(384, dtype=np.float32)


async def _resolve_embedding(context_embedding_b64="", context=""):
    """b64 (if given) wins outright; otherwise embed free-text `context`
    (off the event loop, since it may hit Ollama over the network); zeros
    if neither is given. Without a real context signal here, every call
    embeds the same constant vector, which collapses context-sensitive
    learning (domain inference, novelty, per-domain bleed) into a
    context-free frequency learner."""
    if context_embedding_b64:
        return _decode_embedding(context_embedding_b64)
    if context:
        return await asyncio.to_thread(_get_ap().embed_context, context)
    return np.zeros(384, dtype=np.float32)


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
    ctx: Context = None,
) -> str:
    """MANDATORY before every tool call AND every substantive response — not just tool calls. You MUST call this before selecting a tool OR before reasoning/drafting non-trivial output. Skipping it degrades future suggestions and wastes the learning system."""
    sid = await _ensure_session(session_id)
    actions = [a.strip() for a in available_actions.split(",") if a.strip()] if available_actions else []
    emb = await _resolve_embedding(context_embedding_b64, context)
    result = _get_ap().decide(sid, emb, actions)
    return str(_format_result(result))


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
) -> str:
    """MANDATORY after every tool call AND every substantive response — not just tool calls. You MUST call this immediately after ANY tool finishes OR any non-trivial response is done. Skipping it corrupts the learning signal."""
    sid = await _ensure_session(session_id)
    emb = await _resolve_embedding(context_embedding_b64, context)
    await _get_ap().record_outcome(sid, action_id, reward, emb,
                                   is_blended=is_blended,
                                   blend_edge_ids=[action_id] if is_blended else None)
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
    """Record explicit user feedback. Use when user says 'keep this' or 'don't do this again'."""
    sid = await _ensure_session(session_id)
    emb = await _resolve_embedding(context_embedding_b64, context) if (context_embedding_b64 or context) else None
    annotation = {
        "type": annotation_type,
        "edge_id": action_id,
        "action_id": action_id,
        "intensity": intensity,
    }
    if emb is not None:
        annotation["context_embedding"] = emb
    await _get_ap().record_annotation(sid, annotation)
    return '{"status": "recorded"}'


@mcp.tool(annotations=_READ_ONLY)
async def get_state(session_id: SID = "default") -> str:
    """Full system snapshot: preferences, graph health, ensemble status, novelty health, domain profiles."""
    sid = await _ensure_session(session_id)
    state = _get_ap().get_state()
    return str(state)


@mcp.tool(annotations=_READ_ONLY)
async def list_edges(
    domain: Annotated[str, Field(description="Filter by domain")] = "",
    confidence_min: Annotated[float, Field(description="Minimum confidence threshold")] = 0.0,
    tier: Annotated[str, Field(description="Filter by tier: hot, warm, cold")] = "",
    page: Annotated[int, Field(description="Page number")] = 1,
    per_page: Annotated[int, Field(description="Results per page")] = 20,
) -> str:
    """Browse learned edges in the adaptive graph."""
    kwargs = {"page": page, "per_page": per_page}
    if domain:
        kwargs["domain"] = domain
    if confidence_min:
        kwargs["confidence_min"] = confidence_min
    if tier:
        kwargs["tier"] = tier
    result = _get_ap().list_edges(**kwargs)
    return str(result)


@mcp.tool(annotations=_READ_ONLY)
async def get_edge(
    edge_id: Annotated[str, Field(description="ID of the edge to inspect")] = "",
) -> str:
    """Inspect a specific learned edge in detail."""
    if not edge_id:
        return '{"error": "edge_id required"}'
    edge = _get_ap().get_edge(edge_id)
    if edge is None:
        return '{"error": "edge not found"}'
    return str({
        "id": edge.id,
        "semantic_primitive": edge.semantic_primitive,
        "domain_id": getattr(edge, "domain_id", ""),
        "confidence": getattr(edge, "confidence", 0.5),
        "status": edge.status.value if hasattr(edge.status, "value") else str(edge.status),
        "tier": getattr(edge, "tier", ""),
        "frequency": getattr(edge, "frequency", 0),
        "override_rate": getattr(edge, "override_rate", 0.0),
    })


@mcp.tool(annotations=_READ_ONLY)
async def query_attribution(
    attribution_id: Annotated[str, Field(description="ID of the hint/attribution to explain")] = "",
) -> str:
    """Explain why a hint was suggested. Returns ensemble breakdown, IG score, PC signals, alternatives considered."""
    if not attribution_id:
        return '{"error": "attribution_id required"}'
    raw = _get_ap().query_attribution(attribution_id)
    if raw is None:
        return '{"error": "attribution not found"}'
    result = {
        "edge_id": raw.get("edge_id"),
        "primitive": raw.get("semantic_primitive"),
        "ensemble_mean": raw.get("ensemble_mean", 0.0),
        "ensemble_std": raw.get("ensemble_std", 0.0),
        "ensemble_agree": not raw.get("ensemble_disagree", True),
        "ig_model_score": raw.get("ig_model_score", 0.0),
        "pc_model_score": raw.get("pc_model_score", 0.0),
        "alternatives": raw.get("alternatives_considered", []),
    }
    return str(result)


@mcp.tool(annotations=_READ_ONLY)
async def list_domains() -> str:
    """List all learned domains with DPP diversity weight, novelty lambda, override rate, etc."""
    domains = _get_ap().list_domains()
    return str(domains)


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def toggle_suggestions(
    session_id: SID = "default",
    paused: Annotated[bool, Field(description="True = pause, false = resume")] = True,
) -> str:
    """Pause or resume adaptive suggestions. Learning continues at reduced weight during pause."""
    sid = await _ensure_session(session_id)
    ok = _get_ap().toggle_suggestions(sid, paused)
    if not ok:
        return '{"status": "error", "error": "session not found"}'
    return f'{{"status": "ok", "paused": {str(paused).lower()}}}'


@mcp.tool(annotations=_READ_ONLY)
async def health_check() -> str:
    """Run system diagnostics. Returns health across features, novelty, ensemble, graph, preferences, tier distribution."""
    issues = _get_ap().health_check()
    return str(issues)


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def accept_nudge(session_id: SID = "default") -> str:
    """Accept an exploration nudge to boost alternative approaches for the next few turns."""
    ap = _get_ap()
    sid = await _ensure_session(session_id)
    state = ap._sessions.get(sid)
    mode = state.mode if state else "thought_partner"
    ap._nudge.trigger("User accepted exploration nudge", mode)
    return f'{{"status": "accepted", "active": true, "multiplier": {ap._nudge.multiplier}}}'


@mcp.tool(annotations=_READ_ONLY)
async def session_reflection(session_id: SID = "default") -> str:
    """Session summary: acceptance metrics, top domains, untested approaches."""
    ap = _get_ap()
    result = ap.generate_session_reflection(session_id)
    return str(result)


@mcp.tool(annotations=_WRITES_LOCAL_STATE)
async def resolve_schism(
    keep_faction: Annotated[str, Field(description="a | b | both")] = "both",
) -> str:
    """Resolve an ensemble schism. 'a' keeps faction A, 'b' keeps B, 'both' preserves both with widened variance."""
    try:
        from adaptive_pathway.types import SchismState
        ap = _get_ap()
        if ap._ensemble.schism_state == SchismState.NONE:
            return '{"error": "no active schism"}'
        if ap._ensemble.schism_state == SchismState.DETECTED:
            ap._ensemble.schism_state = SchismState.REVIEWING
        ap._ensemble.resolve(keep_faction)
        return f'{{"status": "resolved", "faction": "{keep_faction}"}}'
    except ValueError as e:
        return f'{{"error": "{str(e)}"}}'


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
- Call `resolve_schism keep_faction="both"` if models have diverged."""


# ── Entry point ─────────────────────────────────────────────────────────
def main():
    parser = argparse.ArgumentParser(description="Adaptive Pathway MCP Server")
    parser.add_argument(
        "--db-path",
        default=os.environ.get("ADAPTIVE_PATHWAY_DB", "./pathway.db"),
        help="Path to the SQLite database (default: ./pathway.db)",
    )
    args = parser.parse_args()
    _get_ap(db_path=args.db_path)
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
