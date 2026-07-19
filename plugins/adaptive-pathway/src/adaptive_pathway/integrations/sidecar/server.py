try:
    import uvicorn
except ImportError:
    uvicorn = None
try:
    from fastapi import FastAPI, HTTPException, Query
    from pydantic import BaseModel
except ImportError:
    FastAPI = None
    HTTPException = None
    Query = None
    BaseModel = None

import asyncio
import logging
import numpy as np
from dataclasses import asdict
from typing import Optional
import time

logger = logging.getLogger("adaptive_pathway.sidecar")


def _quiet_connection_reset_handler(loop, context):
    """Suppress the benign asyncio-on-Windows ConnectionResetError that fires
    when a client (e.g. a health-check probe) disconnects before the
    server-side socket finishes its own teardown. Real errors are still
    passed through to the loop's default handler."""
    exc = context.get("exception")
    if isinstance(exc, ConnectionResetError):
        logger.debug("Suppressed benign ConnectionResetError during transport teardown: %s", exc)
        return
    loop.default_exception_handler(context)


def create_app(adaptive_pathway) -> "FastAPI":
    if FastAPI is None:
        raise ImportError("fastapi is required for the sidecar server")

    app = FastAPI(title="Adaptive Pathway API", version="0.1.0")
    ap = adaptive_pathway

    @app.on_event("startup")
    async def _install_quiet_exception_handler():
        asyncio.get_running_loop().set_exception_handler(_quiet_connection_reset_handler)

    @app.on_event("startup")
    async def _start_maintenance_loop():
        # Nothing previously called run_maintenance() in production — the
        # confidence-decay half-life and cold-edge pruning it drives were
        # fully built but dormant. Runs once the engine has warmed (i.e.
        # after the first session opens; a no-op before that) and then on
        # a fixed interval for the life of the process, no Kitty/client
        # dependency required.
        asyncio.create_task(_maintenance_loop())

    async def _maintenance_loop():
        mc = ap.config.get("maintenance", {})
        interval_s = max(mc.get("interval_hours", 24) * 3600, 60)
        poll_s = mc.get("startup_poll_s", 5)
        while ap._engine is None:
            await asyncio.sleep(poll_s)
        while True:
            try:
                await ap.run_maintenance()
            except Exception:
                logger.exception("Scheduled adaptive-pathway maintenance run failed")
            await asyncio.sleep(interval_s)

    class SessionRequest(BaseModel):
        session_id: Optional[str] = None
        mode: str = "thought_partner"
        domain_hint: Optional[str] = None

    class OutcomeRequest(BaseModel):
        action_id: str
        reward: float
        context_embedding: Optional[list] = None
        # Free-text alternative to context_embedding — embedded server-side
        # (Ollama, falling back to a deterministic hashing vectorizer).
        # Without either, every call looks identical and preferences bleed
        # across unrelated topics (context_embedding wins if both are given).
        context: Optional[str] = None
        is_blended: bool = False
        blend_edge_ids: Optional[list] = None

    class AnnotationRequest(BaseModel):
        type: str
        edge_id: Optional[str] = None
        action_id: Optional[str] = None
        context_embedding: Optional[list] = None
        context: Optional[str] = None
        intensity: float = 0.5

    class EdgeUpdateRequest(BaseModel):
        confidence: Optional[float] = None
        semantic_primitive: Optional[str] = None
        domain: Optional[str] = None
        domain_id: Optional[str] = None
        status: Optional[str] = None
        ttl: Optional[str] = None
        tags: Optional[list] = None
        domain_tags: Optional[list] = None

    class DomainUpdateRequest(BaseModel):
        name: Optional[str] = None
        dpp_diversity_weight: Optional[float] = None
        novelty_lambda: Optional[float] = None
        locked: Optional[bool] = None

    class ImportRequest(BaseModel):
        data: dict
        mode: str = "merge"
        target_domain: Optional[str] = None

    class EnsembleWeightsRequest(BaseModel):
        ig_weight_min: Optional[float] = None
        ig_weight_max: Optional[float] = None
        pc_weight: Optional[float] = None

    async def _ensure_session(session_id: str, mode: str = "thought_partner"):
        if session_id not in ap._sessions:
            await ap.session_open(session_id, mode=mode)
        return session_id

    async def _resolve_embedding(context_embedding=None, context=None):
        """context_embedding (if given) wins outright; otherwise embed
        free-text `context` off the event loop (may hit Ollama over the
        network); zeros if neither is given — an honest "no signal" rather
        than random noise, which would inject spurious context-based
        differentiation between calls that are genuinely context-free."""
        if context_embedding:
            return np.array(context_embedding, dtype=np.float32)
        if context:
            return await asyncio.to_thread(ap.embed_context, context)
        return np.zeros(ap.config["embedding_dim"], dtype=np.float32)

    @app.post("/session/open")
    async def session_open(req: SessionRequest):
        state = await ap.session_open(
            req.session_id or f"sess_{int(time.time())}",
            mode=req.mode,
            domain_hint=req.domain_hint,
        )
        return {
            "session_id": state.session_id,
            "mode": state.mode,
            "opened_at": state.opened_at,
        }

    @app.post("/session/close")
    async def session_close(session_id: str = Query(...)):
        await ap.session_close(session_id)
        return {"status": "closed", "session_id": session_id}

    @app.post("/decide")
    async def decide(
        session_id: str = Query(...),
        context_embedding: str = Query(None),
        context: str = Query(None),
    ):
        await _ensure_session(session_id)
        emb_list = None
        if context_embedding:
            try:
                import base64
                emb_bytes = base64.b64decode(context_embedding)
                emb_list = np.frombuffer(emb_bytes, dtype=np.float32).tolist()
            except Exception:
                emb_list = None
        emb = await _resolve_embedding(emb_list, context)
        result = ap.decide(session_id, emb, [])
        return {
            "hints": [h.text for h in result.hints],
            "confidence": result.confidence,
            "novelty": result.novelty,
        }

    @app.post("/outcome")
    async def record_outcome(session_id: str = Query(...), req: OutcomeRequest = None):
        if req is None:
            raise HTTPException(status_code=400, detail="Request body required")
        await _ensure_session(session_id)
        emb = await _resolve_embedding(req.context_embedding, req.context)
        await ap.record_outcome(
            session_id, req.action_id, req.reward, emb,
            is_blended=req.is_blended, blend_edge_ids=req.blend_edge_ids,
        )
        return {"status": "recorded"}

    @app.post("/annotation")
    async def record_annotation(session_id: str = Query(...), req: AnnotationRequest = None):
        if req is None:
            raise HTTPException(status_code=400, detail="Request body required")
        await _ensure_session(session_id)
        ann = {"type": req.type, "edge_id": req.edge_id,
               "action_id": req.action_id, "intensity": req.intensity}
        if req.context_embedding or req.context:
            ann["context_embedding"] = await _resolve_embedding(req.context_embedding, req.context)
        await ap.record_annotation(session_id, ann)
        return {"status": "recorded"}

    @app.get("/state")
    async def get_state():
        return ap.get_state()

    @app.get("/metrics")
    async def get_metrics(time_range: str = None, domain: str = None):
        return ap.get_metrics(time_range=time_range, domain=domain)

    @app.get("/edges")
    async def list_edges(
        domain: str = None, primitive: str = None,
        confidence_min: float = None, confidence_max: float = None,
        tier: str = None, status: str = None,
        sort: str = "confidence", order: str = "desc",
        page: int = 1, per_page: int = 20,
    ):
        return ap.list_edges(
            domain=domain, primitive=primitive,
            confidence_min=confidence_min, confidence_max=confidence_max,
            tier=tier, status=status,
            sort=sort, order=order, page=page, per_page=per_page,
        )

    @app.get("/edges/{edge_id}")
    async def get_edge(edge_id: str):
        result = ap.get_edge(edge_id)
        if result is None:
            raise HTTPException(status_code=404, detail="Edge not found")
        return {
            "id": result.id,
            "semantic_primitive": result.semantic_primitive,
            "domain_id": result.domain_id,
            "confidence": result.confidence,
            "status": result.status.value,
            "tier": result.tier,
        }

    @app.put("/edges/{edge_id}")
    async def update_edge(edge_id: str, req: EdgeUpdateRequest = None):
        if req is None:
            raise HTTPException(status_code=400, detail="Request body required")
        updates = {}
        if req.confidence is not None:
            updates["confidence"] = req.confidence
        if req.semantic_primitive is not None:
            updates["semantic_primitive"] = req.semantic_primitive
        if req.domain is not None:
            updates["domain"] = req.domain
        if req.domain_id is not None:
            updates["domain_id"] = req.domain_id
        if req.status is not None:
            updates["status"] = req.status
        if req.tags is not None:
            updates["tags"] = req.tags
        if req.domain_tags is not None:
            updates["domain_tags"] = req.domain_tags
        ok = await ap.update_edge(edge_id, updates)
        if not ok:
            raise HTTPException(status_code=404, detail="Edge not found")
        return {"status": "updated"}

    @app.delete("/edges/{edge_id}")
    async def delete_edge(edge_id: str):
        ok = await ap.delete_edge(edge_id)
        return {"status": "deleted" if ok else "not_found"}

    @app.get("/annotations")
    async def list_annotations(
        annotation_type: str = None, domain: str = None,
        time_range: str = None, detection_method: str = None,
        page: int = 1, per_page: int = 20,
    ):
        return ap.list_annotations(
            annotation_type=annotation_type, domain=domain,
            time_range=time_range, detection_method=detection_method,
            page=page, per_page=per_page,
        )

    @app.get("/domains")
    async def list_domains():
        return ap.list_domains()

    @app.put("/domains/{domain_id}")
    async def update_domain(domain_id: str, req: DomainUpdateRequest = None):
        if req is None:
            raise HTTPException(status_code=400, detail="Request body required")
        updates = {}
        if req.name is not None:
            updates["name"] = req.name
        if req.dpp_diversity_weight is not None:
            updates["dpp_diversity_weight"] = req.dpp_diversity_weight
        if req.novelty_lambda is not None:
            updates["novelty_lambda"] = req.novelty_lambda
        if req.locked is not None:
            updates["locked"] = req.locked
        ok = ap.update_domain(domain_id, updates)
        if not ok:
            raise HTTPException(status_code=404, detail="Domain not found")
        return {"status": "updated"}

    @app.post("/domains/{domain_id}/reset")
    async def reset_domain(domain_id: str, mode: str = "soft"):
        ok = ap.reset_domain(domain_id, mode=mode)
        return {"status": "reset" if ok else "not_found"}

    @app.post("/graph/export")
    async def export_graph(include_annotations: bool = False,
                          include_ensemble_state: bool = False,
                          domain: str = None):
        return ap.export_graph(
            include_annotations=include_annotations,
            include_ensemble_state=include_ensemble_state,
            domain=domain,
        )

    @app.post("/graph/import")
    async def import_graph(req: ImportRequest = None):
        if req is None:
            raise HTTPException(status_code=400)
        ok = ap.import_graph(req.data, mode=req.mode, target_domain=req.target_domain)
        return {"status": "imported" if ok else "failed"}

    @app.get("/attribution/{attribution_id}")
    async def query_attribution(attribution_id: str):
        result = ap.query_attribution(attribution_id)
        if result is None:
            raise HTTPException(status_code=404, detail="Not found")
        return result

    @app.post("/suggestions/toggle")
    async def toggle_suggestions(session_id: str = Query(...), paused: bool = True):
        ok = ap.toggle_suggestions(session_id, paused)
        return {"status": "toggled" if ok else "session_not_found"}

    @app.post("/micro_annotation")
    async def submit_micro_annotation(
        session_id: str = Query(...),
        atype: str = Query(...),
        action_id: str = Query(...),
    ):
        ok = ap.submit_micro_annotation(session_id, atype, action_id)
        return {"status": "submitted" if ok else "suppressed"}

    @app.post("/nudge/dismiss")
    async def dismiss_nudge():
        ap.dismiss_nudge()
        return {"status": "dismissed"}

    @app.post("/nudge/accept")
    async def accept_nudge(session_id: str = Query(...)):
        state = ap._sessions.get(session_id)
        mode = state.mode if state else "thought_partner"
        ap._nudge.trigger("User accepted exploration nudge", mode)
        return {"status": "accepted", "active": True, "multiplier": ap._nudge.multiplier}

    @app.get("/health")
    async def health_check():
        issues = ap.health_check()
        return {"issues": [i if isinstance(i, dict) else {"message": str(i)} for i in issues]}

    @app.get("/graph_health")
    async def graph_health():
        return asdict(ap.get_graph_health())

    @app.post("/maintenance")
    async def run_maintenance():
        await ap.run_maintenance()
        return {"status": "maintenance_complete"}

    @app.get("/schism")
    async def get_schism():
        alert = ap.get_schism_alert()
        if alert is None:
            return {"state": "none"}
        return alert

    @app.post("/schism/resolve")
    async def resolve_schism(keep_faction: str = Query(...)):
        ok = ap.resolve_schism(keep_faction)
        return {"status": "resolved" if ok else "no_active_schism"}

    @app.get("/session_reflection")
    async def session_reflection(session_id: str = Query(...)):
        return ap.generate_session_reflection(session_id)

    @app.put("/config/ensemble")
    async def update_ensemble_weights(req: EnsembleWeightsRequest):
        result = ap.update_ensemble_weights(
            ig_weight_min=req.ig_weight_min,
            ig_weight_max=req.ig_weight_max,
            pc_weight=req.pc_weight,
        )
        if "error" in result:
            raise HTTPException(status_code=400, detail=result["error"])
        return result

    return app


def run_server(adaptive_pathway, host="127.0.0.1", port=8700):
    if uvicorn is None:
        raise ImportError("uvicorn is required for the sidecar server")
    app = create_app(adaptive_pathway)
    uvicorn.run(app, host=host, port=port)


__all__ = ["create_app", "run_server"]
