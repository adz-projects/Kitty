# Adaptive Pathway — ACP Endpoints

This document describes a conceptual ACP-protocol-shaped endpoint surface. The endpoints that
are actually implemented today are exposed as plain REST routes by the FastAPI sidecar
(`src/adaptive_pathway/integrations/sidecar/server.py`) and as MCP tools
(`src/adaptive_pathway/mcp_server.py`) — see `api-reference.md` for the underlying Python API
those two integrations wrap. 15 of the endpoints below are implemented; `set_mode` and
`set_preferences` are NOT implemented anywhere in this codebase (verified: no `set_mode`/
`set_preferences` route, tool, or method exists) and are left here as a planned/aspirational
design note.

## `set_mode` — **NOT IMPLEMENTED**
**Payload:** `{ "mode": "thought_partner" | "agent" }`

Agent mode multiplies `novelty_lambda` by 0.5 across all domains. Less curiosity, more efficiency.
Today, mode is set once via `session_open(session_id, mode=...)` and is not changeable mid-session.

---

## `set_preferences` — **NOT IMPLEMENTED**
**Payload:** `{ "bleed_temperature": 0.0..1.0, "post_session_summary": "minimal"|"verbose"|"off", "auto_ttl_enabled": bool, "domain": "infer"|"<tag>", "shadow_mode": bool, "domain_overrides": { "<id>": { "dpp_diversity_override": float, "novelty_lambda_override": float } } }`

Full preferences update. Domain overrides take precedence over inferred values. Clearing an override restores inference.
Today, the closest equivalents are `PUT /config/ensemble` (ensemble weights) and `PUT /domains/{id}` (per-domain DPP/lambda overrides).

---

## `get_state`
**Payload:** `{}`

Returns full system snapshot: preferences, graph health, ensemble health (schism_state, diversity_mode), novelty health, domain profiles (with DPP/lambda/rates), pause statistics, discovery status. Powers the settings panel.

---

## `get_metrics`
**Payload:** `{ "metrics": ["override_rate", ...], "time_range": "7d"|"14d"|"30d"|"all", "domain_filter": "<id>"|null }`

Returns requested metrics with values and trends. Powers dashboards. Unknown metric names return null — front-end can probe.

---

## `list_edges`
**Payload:** `{ "domain_filter", "primitive_filter", "confidence_min", "confidence_max", "tier_filter", "status_filter", "sort_by", "sort_order", "page", "per_page" }`

Paginated edge listing. Powers graph visualizations and curation tools.

---

## `get_edge`
**Payload:** `{ "edge_id": "..." }`

Full detail: semantic primitive, domain, confidence, status, decision history, annotations, related edges.

---

## `update_edge`
**Payload:** `{ "edge_id": "...", "updates": { "confidence": 0.5, "semantic_primitive_rename": "...", "domain_reassign": "...", "status": "established", "ttl_clear": true } }`

Manual curation. All fields optional. `semantic_primitive_rename` updates all edges sharing that primitive.

---

## `delete_edge`
**Payload:** `{ "edge_id": "..." }`

Archives to cold tier. Reversible via rollback.

---

## `list_annotations`
**Payload:** `{ "type_filter": [...], "domain_filter", "time_range", "detection_method", "page", "per_page" }`

Paginated annotation history with intensity, detection confidence, behavioral confirmation. Powers annotation browsers.

---

## `list_domains`
**Payload:** `{}`

All domains with full profiles: DPP diversity, novelty lambda, revision rate, acceptance rate, sessions, edge count, override rate, source, lock status.

---

## `update_domain`
**Payload:** `{ "domain_id": "...", "updates": { "name": "...", "dpp_diversity_override": 1.2, "novelty_lambda_override": 0.8, "lock_inferred": true } }`

`lock_inferred: true` prevents weekly re-inference from overwriting manual values.

---

## `reset_domain`
**Payload:** `{ "domain_id": "...", "mode": "soft"|"hard" }`

Soft: halves A_inv/b matrices and edge confidences. Hard: zeroes everything. Powers format-change workflows.

---

## `export_graph`
**Payload:** `{ "domain_filter", "include_annotations": true, "include_ensemble_state": false }`

Exports learned graph as JSON. `include_ensemble_state: true` includes A_inv/b matrices for full backup.

---

## `import_graph`
**Payload:** `{ "import_data": {...}, "mode": "merge"|"replace_domain"|"replace_all", "target_domain": "<id>" }`

Merge adds new edges, updates existing if imported confidence is higher. Replace modes archive existing data first.

---

## `query_attribution`
**Payload:** `{ "attribution_id": "..." }`

Full Thompson decomposition for a specific hint: posterior distribution, ensemble agreement (4 models), IG score, PC model signals (domain isolation, confidence gap, primitive isolation, novelty persistence), DPP rank, decision history, alternatives considered.

---

## `toggle_suggestions`
**Payload:** `{ "paused": true|false }`

Session-scoped. Auto-lifts at session close. Front-end shows "Suggestions paused — learning continues." Learning proceeds at 0.6× weight.

---

## `submit_micro_annotation`
**Payload:** `{ "annotation_type": "micro_positive"|"micro_negative"|"explore_alternative"|"retry_same_intent", "edge_ids": [...] }`

👍/👎/💡/🔄 from hover menu. Ignored when suggestions paused.