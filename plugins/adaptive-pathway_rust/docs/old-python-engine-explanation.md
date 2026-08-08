> **Historical.** Describes the retired Python tool-selection engine (the
> contextual-bandit sidecar + stdio MCP proxy, `plugins/adaptive-pathway/`,
> since deleted from the tree). It has been fully replaced by the in-process
> behavioral-memory engine in this crate — see `../architecture.md` and
> `behavioral-memory-plan.md` for the current design. Kept only as the
> "what the old system actually did" oracle mentioned in
> `plugins/build.py`'s module doc comment.

# Adaptive Pathway — Detailed Explanation (retired Python engine)

## Architecture Overview

The plugin implements a **context-aware reinforcement learning system** that learns which tools and approaches a user prefers over time. It has two processes:

1. **Sidecar** (`integrations/sidecar/`) — A FastAPI HTTP server on port 8700 that owns the single `AdaptivePathway` engine instance and the SQLite database.
2. **MCP Server** (`mcp_server.py`) — A stateless stdio proxy that forwards MCP tool calls to the sidecar via HTTP. This avoids split-brain (two processes writing the same DB).

The core engine lives in `engine.py` (~1750 lines) and orchestrates ~30 submodules organized into four packages: `decision/`, `discovery/`, `learning/`, `storage/`.

---

## Step-by-Step Flow

### 1. Initialization

```
AdaptivePathway(db_path, config_path, **overrides)
```

Loads `config/defaults.yaml` (264 lines of tunable parameters), then instantiates all subcomponents:

| Component | Module | Purpose |
|---|---|---|
| `EmbeddingProvider` | `embeddings.py` | Converts text → 384-dim vector via Ollama, falling back to deterministic feature hashing (MurmurHash3 with signed buckets) |
| `FeatureHasher` | `features.py` | Hashes action IDs + metadata into 64-bucket sparse vectors for Thompson sampling |
| `ActionBucketer` | `features.py` | Maps semantic primitives → 64 action buckets (Thompson arms) |
| `BootstrapEnsemble` | `decision/ensemble.py` | 6-model ensemble: 4× ThompsonLinUCB + 1× InformationGain + 1× ParadigmChallenge |
| `CountBasedNovelty` | `decision/novelty.py` | Count-Min Sketch (3 hash tables × 2048 buckets) tracking how often similar contexts have been seen |
| `CuriosityNudge` | `learning/curiosity.py` | Detects action-entropy plateaus and offers exploration boosts |
| `PreferenceDetector` | `learning/preferences.py` | Learns positive/negative centroid embeddings from user feedback |
| `EdgeTTL` | `learning/ttl.py` | Suppresses edges after crashes or explicit rejection for configurable durations |
| `DomainBleed` | `learning/bleed.py` | Dampens cross-domain suggestions via temperature-based exponential decay |
| `PrimitiveDiscoverer` | `discovery/primitives.py` | Tracks co-occurrence of semantic primitives across sessions |
| `DomainDiscovery` | `discovery/domains.py` | Auto-discovers up to 8 domains via centroid clustering of context embeddings |
| `TieredCache` | `storage/tiered.py` | Hot/warm/cold edge cache with in-memory vector index |
| `VectorIndex` | `storage/vec.py` | Amortized-doubling cosine-similarity search over edge embeddings |

### 2. Session Open

```
await session_open(session_id, mode="thought_partner", domain_hint=None)
```

- Ensures SQLite DB is initialized (`storage/database.py` — 14 tables including `edges`, `nodes`, `annotations`, `ensemble_state`, `ttl_entries`, etc.)
- **Warm load**: Reads all hot/warm-tier edges from DB into `_edge_index` (bucketed in-memory dict), loads embeddings into `VectorIndex`, restores ensemble `A_inv`/`b` matrices (binary-packed as BLOBs, ~16KB per 64×64 matrix vs ~60KB as JSON), restores TTL entries, domain centroids, and preference detector centroids
- Creates a per-session `InSessionBandit` (recency-weighted Thompson bandit with exponential decay, max mix weight 0.20 after 20 calls) and an `ActionSelector`

### 3. Decision (Hot Path — Synchronous, Sub-10ms)

```
decide(session_id, context_embedding, available_actions) → DecisionResult
```

This is the critical path. It never touches disk:

1. **Domain inference**: If no `domain_hint`, computes cosine similarity between the context embedding and each known domain centroid → picks the closest domain (threshold ≥ 0.6)

2. **Edge retrieval**: Buckets the available actions, fetches candidate edges from `TieredCache._hot`

3. **TTL filtering**: Removes edges with active TTL (crash or user-rejected suppressions)

4. **Ensemble scoring** (per edge, memoized per bucket):
   - **4× ThompsonLinUCB models**: Draw from posterior `N(θ̂, σ²A_inv)` using cached Cholesky factorization for speed. Each model applies bootstrap probability (80%) — not all 4 update on every reward, creating diversity.
   - **Information Gain model**: Scores by how much variance reduction an action would provide (`σ_before - σ_after`), passed through a sigmoid. Weight scales with plateau risk (0.15–0.50).
   - **Paradigm Challenge model**: Composite score of 4 signals — domain isolation (is this edge from an underrepresented domain?), confidence gap (is this domain less confident than the top domains?), primitive isolation (no co-selection with referent edges), novelty persistence. Weight: 0.15.

5. **In-session mixing**: Blends ensemble score with in-session bandit score at increasing weight (0→0.20 over 20 calls)

6. **Novelty bonus**: `λ / (1 + min_count)` from Count-Min Sketch, plus per-action UCB bonus

7. **Domain bleed**: Cross-domain edges multiplied by `exp(-1/temperature)` (default temp=0.3, so cross-domain edges get ~4% weight)

8. **DPP selection** (`decision/diversity.py`): Builds a kernel `W @ similarity @ W` over top candidate embeddings, then greedy k-submodular maximization to select diverse hints (up to 5). This prevents suggesting 5 semantically similar approaches.

9. **Special slots**:
   - **Uncertainty slot**: Picks the edge with highest posterior σ among non-selected edges — guarantees one "we genuinely don't know" hint
   - **Wildcard slot**: Scores remaining edges by PC score (70%) + novelty (30%), picks one above threshold — surfaces paradigm-gap approaches

10. **Blending** (`decision/blending.py`): Finds pairs of high-confidence edges sharing a domain, creates `BlendedHint` suggestions combining them

11. **Plateau risk evaluation** (every 15 calls): 4-signal decomposition — entropy trend, diversity decline, novelty acceleration, ensemble agreement collapse. Rising plateau risk increases IG model weight.

12. **Co-selection tracking**: Records which primitives appear together in the same hint set (persisted at session close)

### 4. Feedback Loop

```
await record_outcome(session_id, action_id, reward, context_embedding, ...)
```

1. **Ensemble update**: Calls `update()` on all 6 models for the relevant action bucket(s). Bootstrap: only 80% of standard models actually update (creates diversity). The IG model always updates. The PC model has no learned state.

2. **In-session bandit update**: Recency-weighted buffer (half-life = 5 calls, max 15 entries). Rebuilds from scratch each time with exponential decay on older rewards.

3. **Graph edge touch** (`_touch_edge`): Upserts the semantic primitive in the in-memory `_edge_index`. Increments frequency, adjusts confidence by `base_step × reward` (±0.10), promotes to ESTABLISHED after 3 provisional successes. Creates new edges with their context embedding.

4. **Persistence**: Writes action history, novelty history, ensemble state (binary-packed `A_inv`/`b`), edge row, and TTL store to SQLite within a single transaction.

5. **Primitive discovery**: Every 50 calls, extracts co-occurrence patterns from the current edge set.

6. **Novelty tracking**: Visits the context in Count-Min Sketch, appends novelty score to history.

### 5. Explicit Annotations

```
await record_annotation(session_id, {type, edge_id, context_embedding, intensity})
```

Handles 6 annotation types:

| Type | Effect |
|---|---|
| `keep_this` | +0.40 to +0.80 reward (scales with embedding centroid similarity) |
| `dont_do_again` | -0.30 to -0.60 reward + **TTL suppression for 30 days** (moderate+) |
| `micro_positive` | +0.10 (capped per session at 0.25 total) |
| `micro_negative` | -0.06 (capped) |
| `explore_alternative` | Triggers curiosity nudge |
| `retry_same_intent` | +0.10 mild reward |

The `PreferenceDetector` accumulates labeled examples and builds positive/negative centroid embeddings (after 50+ positive examples). Once ready, it uses cosine similarity against these centroids to auto-detect implicit preferences from context embeddings alone — no explicit annotation needed. Strong rejections trigger a novelty lambda boost for subsequent sessions.

### 6. Curiosity Nudge System

Every 50 decide calls, `CuriosityNudge` checks action entropy over the last 50 turns:
- If entropy < 0.3 AND top-3 concentration > 0.85 → plateau detected
- Offers a nudge: multiplier ×1.5 on novelty lambda for 10 turns
- User accepts via `accept_nudge()` or dismisses (14-day cooldown)
- Blocked if user is already exploring actively, or in agent mode

### 7. Ensemble Schism Detection

Every 25 updates (with 4-hour minimum between checks), the ensemble computes a pairwise agreement matrix across the 4 standard Thompson models — fraction of predictions within 0.15 of each other over the last 10 calls. It then searches all bipartitions of models for factions where:
- Within-faction agreement > 0.80
- Between-faction agreement < 0.40

If found and not a simple domain split, it enters `DETECTED` state → `REVIEWING` (decide returns empty hints) → user resolves via `resolve_schism(keep_faction)` — either keep one faction (copy winning models to losers), or keep both (widen all variances by 1.3×).

### 8. Domain Discovery

Up to 8 domains auto-discovered via centroid clustering:
- Context embeddings that don't match any existing domain centroid are pooled
- Every 10 sessions, if the pool has ≥10 unassigned embeddings, a cluster center is estimated (highest mean-similarity point)
- A new domain is created with that centroid
- Centroids update via online EMA (α=0.05) as new contexts arrive

### 9. Tier Management & Maintenance

Edges have three tiers:
- **Hot**: Recently accessed, or confidence ≥ 0.80 — kept in fast in-memory cache
- **Warm**: In DB with embeddings, loaded at startup for vector search
- **Cold**: Archived after 180 days of inaccess — pruned during maintenance

Nightly maintenance (`run_maintenance()`):
- Applies confidence decay (half-life = 168 hours, max 30% decay) by blending `A_inv` toward identity — old preferences must re-earn confidence
- Purges old action/novelty history rows
- Demotes warm→cold edges past threshold
- Prunes expired TTL entries
- Syncs domain state to DB

### 10. Storage Layer

14 SQLite tables via SQLAlchemy async:
- `edges` — semantic primitives with confidence, domain, tier, frequency, co-selection lists
- `nodes` — context embeddings (BLOB) linked to edges
- `ensemble_state` — binary-packed `A_inv`/`b` matrices per model per action bucket
- `annotations` — explicit feedback history
- `ttl_entries` — active suppressions with expiry
- `domains` — auto-discovered domain centroids and metadata
- `feedback_centroids` — preference detector state
- `action_history` / `novelty_history` — bounded time series
- `blended_edge_log` / `co_selection_log` — relationship tracking
- `app_settings` — KV store for user-tunable weights

### 11. Integration Layer

**Sidecar** (`integrations/sidecar/server.py`): FastAPI app with ~20 REST routes. Starts a background maintenance loop. Serializes all structured types to JSON. Handles embedding resolution (b64 → float32, or free-text → Ollama).

**MCP Server** (`mcp_server.py`): Thin stdio proxy using `FastMCP`. Every tool maps to one HTTP call to the sidecar. Exposes 15 tools: `decide`, `record_outcome`, `record_annotation`, `get_state`, `list_edges`, `get_edge`, `query_attribution`, `list_domains`, `toggle_suggestions`, `health_check`, `accept_nudge`, `session_reflection`, `resolve_schism`, `session_close`. Also provides an `adaptive_instructions` prompt that tells the LLM how to use the system.

### 12. Configuration

All parameters live in `config/defaults.yaml` (264 lines). Key tunables:
- Ensemble weights (`ig_weight_min/max`, `pc_weight`) — adjustable via `update_ensemble_weights()`
- Novelty lambda per domain and mode (agent mode halves it)
- DPP max hints (5), wildcard slots (1), blend pairs (2)
- Thompson buckets (64×64 matrices × 6 models ≈ 12MB in memory)
- Tier thresholds, decay half-lives, TTL durations
- Curiosity nudge sensitivity and cooldown

---

## Key Design Principles

1. **Single engine ownership**: The sidecar owns the engine; the MCP server is stateless. No split-brain.
2. **Sync hot path**: `decide()` is synchronous with zero I/O — all data pre-loaded in memory.
3. **Binary persistence**: Ensemble matrices stored as packed binary (16KB) not JSON (60KB).
4. **Cholesky caching**: Thompson sampling uses cached Cholesky factors, invalidated only on update — cuts decide latency ~3×.
5. **Bucket memoization**: Ensemble draws are cached per action bucket within a single `decide()` call — edges sharing a bucket don't re-sample.
6. **Graceful degradation**: Ollama unavailable → deterministic hashing embeddings. DPP fails → top-k fallback. Schism detected → empty hints until resolved.
