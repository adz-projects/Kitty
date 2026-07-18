# Adaptive Pathway — Python Module API

The core package (`adaptive-pathway`) exposes a single main class with ~25 methods organized into four groups.

## Lifecycle

### `AdaptivePathway(db_path, config_path, **overrides)`
Constructor. Loads `defaults.yaml`, applies overrides. Does not touch disk beyond loading config. Cheap — no I/O.

### `await session_open(session_id, mode, domain_hint)`
Async. Pre-loads all data: hot-tier edges, VectorIndex, ensemble state, novelty hash tables, cross-session histories. Creates per-session `InSessionBandit` and `ActionSelector`. Returns `SessionState`.

### `await session_close(session_id)`
Async. Replays deferred annotations. Resets in-session bandit. Persists co-selection data.

---

## Decision (Hot Path — Synchronous, Sub-10ms)

### `decide(session_id, context_embedding, available_actions) → DecisionResult`
Synchronous. Never touches disk. Reads only from in-memory data. Returns:
- `hints` — DPP-selected list of `Hint`/`BlendedHint` instances
- `confidence`, `novelty`, `is_flow_state`
- `plateau_risk` — `PlateauRisk` with 4-signal decomposition
- `in_session` — `InSessionStatus` with mix weight and call count
- `nudge_active` — `NudgeStatus` if coarse nudge is active
- `schism_alert` — `SchismAlert` if ensemble split detected

---

## Feedback (Async)

### `await record_outcome(session_id, action_id, reward, context_embedding, is_blended, blend_edge_ids)`
Async. Updates ensemble (bootstrap), in-session bandit (recency-weighted buffer), cross-session histories, and SQLite immediately. Splits reward across both source edges for blended actions.

### `await record_annotation(session_id, annotation, paused_replay)`
Async. Applies explicit user annotation (keep_this, dont_do_again, micro_positive, etc.) to the ensemble.

---

## Query & Curation

### `get_state() → dict`
Returns full system state: preferences, graph health, ensemble health (schism state, diversity mode), novelty health, domain profiles, pause statistics, discovery status.

### `get_metrics(metrics, time_range, domain_filter) → MetricsResult`
Arbitrary metrics: override_rate, path_success_rate, confidence_distribution, annotation_counts, novelty_distribution, domain_usage, pause_frequency, top_overridden_edges. Trend over time_range.

### `list_edges(domain_filter, primitive_filter, confidence_min, confidence_max, tier_filter, status_filter, sort_by, sort_order, page, per_page) → dict`
Paginated edge listing. Returns `edges`, `total`, `page`, `pages`.

### `get_edge(edge_id) → EdgeInfo`
Full edge detail including decision history, annotations, related edges.

### `update_edge(edge_id, updates) → bool`
Manual modification: confidence, primitive rename, domain reassign, status, TTL clear.

### `delete_edge(edge_id) → bool`
Archives edge to cold tier. Reversible via rollback.

### `list_annotations(type_filter, domain_filter, time_range, detection_method, page, per_page) → dict`
Paginated annotation history. Returns `annotations`, `total`, `page`, `pages`.

### `list_domains() → list[DomainProfile]`
All domains with DPP diversity weight, novelty lambda, source, inference stats.

### `update_domain(domain_id, updates) → bool`
Modify domain: name, DPP override, novelty lambda override, lock inferred.

### `reset_domain(domain_id, mode) → bool`
Soft reset: halves A_inv/b matrices and edge confidences. Hard reset: zeroes everything.

### `export_graph(domain_filter, include_annotations, include_ensemble_state, format) → dict`
Export learned graph as JSON for backup/transfer.

### `import_graph(import_data, mode, target_domain) → bool`
Import: merge (add new), replace_domain, replace_all (archive existing).

### `query_attribution(attribution_id) → Attribution`
Full Thompson decomposition: posterior, ensemble agreement, novelty, DPP rank, decision history, PC model signals.

### `toggle_suggestions(session_id, paused) → bool`
Pause/resume hint injection. Learning continues at 0.6× weight during pause.

### `submit_micro_annotation(session_id, annotation_type, action_id) → bool`
👍/👎/💡/🔄 feedback from UI.

### `dismiss_nudge()`
Cancels active curiosity nudge. Cooldown prevents re-trigger for 14 days.

### `health_check() → list[HealthIssue]`
Circular dependency detection, confidence inversion, feature collision, RND staleness, ensemble schism, centroid staleness.

### `await run_maintenance()`
Nightly: purge old history rows, prune stale edges, flush hash tables, refresh centroids, re-infer domain profiles.