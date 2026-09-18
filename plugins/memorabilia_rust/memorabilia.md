# Memorabilia

A single-user **declarative factual memory** engine, written in Rust. It ingests
evidence (documents, conversation exchanges), distills atomic factual claims from
it, tracks how confident it is in each claim and when time-sensitive claims
expire, and — at query time — serves back a small, token-capped slice of the most
relevant facts. It is designed to sit *inside* another program's turn loop: its
primary runtime is a compiled-in plugin of the **BigTiny V2** daemon, where it
(a) injects a compact summary + index of relevant memory into a model's context
before each turn and (b) exposes MCP tools the model can call to pull any item in
full.

This document describes the whole system end to end: what it does, how the pieces
fit, the exact formulas and thresholds, where each thing lives in the source, and
the invariants you must not break. It is meant to be read by a person as a
reference and by an LLM as a map during edits and bug fixes.

> Older design notes (`project-plan.md`, `claude.md`, `AGENTS.md`,
> `size-control-plan.md`) have been moved to the git-ignored `docs_old/` and are
> superseded by this file. Where they disagree with the code, the code wins; this
> document tracks the code.

---

## 1. Mental model

Memory is stored at **two levels**, and the split is the single most important
idea in the system:

- **Level 0 — Evidence chunks** (`chunks` table). Raw text snippets, each with a
  source, a citation, a vector embedding, and a **decay profile**. Chunks are the
  only things that carry time: a decay class, time constants, an anchor timestamp,
  and reinforcement/grounding counts. Chunks age, get reinforced, and eventually
  archive and are deleted.
- **Level 1 — Propositions** (`propositions` table). Atomic factual claims
  extracted from chunks by a language model ("Python lists are mutable"). A
  proposition holds **no timers**. Its `confidence` is *derived* from its active
  supporting chunks, and it lives only as long as at least one active chunk
  supports it.

The rule that follows from this: **decay and lifecycle belong to chunks, never to
propositions.** When a proposition's active support drops to zero it leaves the
graph (archived if important, deleted otherwise). Confidence is always a function
of *current* supporting evidence, never a stored opinion that drifts on its own.

Everything else — retrieval, disputes, reinforcement, promotion — is built on top
of that two-level structure.

Two cross-cutting disciplines:

- **Soft-fail everywhere.** Any DB/LLM/embedder error inside ingestion, recall, or
  maintenance is logged (`tracing::warn`) and swallowed. When the engine has
  nothing to say, recall returns `None` and the caller's prompt is byte-identical
  to no-memory behavior. A memory failure never breaks the host's turn.
- **No magic numbers.** Every threshold, time constant, and cap is a named field
  in one `Config` struct (`src/config.rs`) with a default and a rationale comment.
  Tests inject overrides through `Config`; production loads one YAML file.

---

## 2. How it runs

The crate is **host-agnostic**. It depends only on trait *seams*, never on
concrete providers, so the same `Engine` runs in three contexts:

1. **Tests** — in-memory SQLite (`Db::open_in_memory`), a `MockChat`, and the
   deterministic hash embedder. No network, fully reproducible.
2. **The BigTiny V2 plugin** (the real runtime) — the daemon injects adapters over
   its shared EmbeddingGemma embedder and its `SummarizerChain` (Gemma E2B →
   configured provider), and opens one engine per app. See §14.
3. Any future host that implements the three seams.

There is **no standalone Memorabilia daemon or binary** — it is a library
(`[lib]` only). The BigTiny plugin is the sole production host.

### The seams (`src/traits.rs`)

```rust
trait StructuredChat  { async fn structured_chat(msgs: Vec<Value>, schema: &Value) -> Result<Value,String>; }
trait Embedder        { async fn embed(text: &str) -> Result<(Vec<f32>, String), String>;   // (vector, space-tag)
                        async fn embed_fresh(text: &str) -> ...;                              // cache-bypassing
                        fn model_tag(&self) -> &str; }
trait VectorIndex     { async fn upsert(chunk_id, embedding, model); remove; search(q, model, k); list_by_model(model); }
```

- `StructuredChat` is the **extraction/maintenance LLM** — JSON-schema-constrained
  completions. It is never the calling/host LLM; in production it's Gemma E2B via
  BigTiny's `SummarizerChain`. Test double: `MockChat`.
- `Embedder` returns a vector **and the tag of the space that produced it**. A
  fallback-aware embedder may return the lexical-hash tag even though its primary
  is semantic — callers persist and filter by the returned tag, never by static
  config. Implementations: `OllamaEmbedder` (semantic + hash fallback,
  `src/embed/provider.rs`), `HashEmbedder` (deterministic lexical,
  `src/embed/hashing.rs`).
- `VectorIndex` is brute-force cosine over active chunks. Implementations:
  `SqliteVectorIndex` (sqlite-vec `vec0`, `src/store/vectors.rs`) and
  `MemoryVectorIndex` (in-memory test double).

The `Engine` struct (`src/engine.rs`) holds `config`, `db`, and `Arc<dyn>` handles
to all three seams, plus the reliability data, an optional PII classifier, and the
global extraction semaphore. Two constructors:

- `Engine::new(config, db, chat, embedder, vectors)` — sync, for tests that built
  their own `Db`.
- `Engine::open_at(path, config, chat, embedder) -> Arc<Engine>` — async; opens the
  SQLite DB, runs migrations, sizes the vector index to `config.embedding_dim`, and
  wires `SqliteVectorIndex`. This is what the BigTiny host calls.

### Vector-space safety

Two embedding spaces exist and must **never** cosine-compare against each other:
the semantic model's space (tagged e.g. `qwen3-embedding:0.6b` or, under BigTiny,
`litert:embeddinggemma`) and the deterministic lexical fallback
(`__lexical_hash__`, `config::HASH_EMBED_MODEL`). Every vector row is tagged, and
`VectorIndex::search` filters by exact tag. A regression guard that a
`__lexical_hash__` vector never matches a semantic query is load-bearing.

---

## 3. Storage & schema

SQLite, single connection (so the manual `BEGIN`/`COMMIT` in
`Db::run_in_transaction` is actually atomic), WAL + `foreign_keys=ON`. The vector
index is `sqlite-vec`'s `vec0` virtual table, created in code (not a migration)
because the `float[dim]` width depends on `config.embedding_dim`.

Migrations (`migrations/`):
- `001_init.sql` — `app_settings(key, value)`.
- `002_schema.sql` — the full schema (below).
- `003_extraction_retry.sql` — adds `chunks.extraction_error_at`.

Tables and what they hold (`src/store/*.rs` owns the row types + parameterized
SQL; **no business logic lives in the store layer**):

| Table | Module | Key columns / purpose |
|---|---|---|
| `documents` | `documents.rs` | one row per ingested payload; FK parent of its chunks; `document_hash`, final `chunk_count`. |
| `chunks` | `chunks.rs` | Level 0. `content`, `content_hash`, `document_hash`, `source_entity`, `source_reliability`, `provenance_cluster_id`, `cluster_citation`, `status` (`active`→`archived`), `extraction_status` (`pending`/`done`), `extraction_error_at`, `embedding_model` (space tag), `decay_class` (`static`/`transient`/`deadline`), `anchor_at`, `urgency_expires_at`, `reinforcement_count`, `grounding_count`, `archived_at`. |
| `chunks_vec` | `vectors.rs` (vec0) | `chunk_id → embedding float[dim]`. Created by `Db::init_vectors(dim)`. |
| `propositions` | `propositions.rs` | Level 1. `node_id`, `claim`, derived `confidence`, derived `is_disputed`, `status` (`active`/`UNSUPPORTED_ARCHIVE`), `importance`, `urgency`, `urgency_expires_at`, `last_assessed_at`, `archived_at`. |
| `chunk_propositions` | `propositions.rs` | support links `(chunk_id, node_id)`; the join that maps evidence ↔ claim. |
| `proposition_edges` | `edges.rs` | typed edges between propositions: `DEPENDS_ON`, `CAUSES`, `BLOCKS`, `MODIFIES_DEADLINE`, `RHYMES_WITH`; `weight` (≤ 0.8, used by spreading activation); optional TTL for `RHYMES_WITH`. |
| `disputed_edges` | `edges.rs` | the **one** dispute mechanism: a symmetric edge between two chunks, canonical order `chunk_a < chunk_b` (CHECK), `UNIQUE(chunk_a, chunk_b)`, `strength`, `opened_at`, `closed_at`. |
| `source_registry` | `registry.rs` | per-`source_entity` reliability: `tier`, `tier_prior`, drifted `reliability`. |
| `reinforcement_outbox` | `outbox.rs` | `(chunk_id, query_event_id, grounded, reinforced)`, `UNIQUE(chunk_id, query_event_id)`. Written in the recall transaction, drained by maintenance. |
| `tombstones`, `suppressions` | `tombstones.rs` | forget-ladder state (§13). |
| `audit_log` | `audit.rs` | event log; records PII **category only**, never values. |

Every table reserves a nullable `owner_id` so multi-owner support wouldn't force a
migration later (the system is single-user today).

`extraction_watermark` (in `app_settings`) is a forward-only rowid bound: the
highest chunk rowid ever taken to `done`. `last_maintenance_at` (also
`app_settings`) gates the 24-hour heavy maintenance pass.

---

## 4. Configuration (`src/config.rs`)

One `Config` struct is the single source of truth. All config lives in one YAML
file loaded by `Config::load(Option<&Path>)`: no file → all defaults; a present
file overrides only the keys it sets; unknown keys are ignored; malformed YAML is
an error. A sample with every knob is `config.yaml` at the repo root.

Selected defaults (see the file for the full table and rationale comments):

- **Embedding**: `embedding_dim=384`, `embedding_model="qwen3-embedding:0.6b"`,
  `embedding_provider_url="http://localhost:11434"`, `embedding_timeout_ms=1500`,
  `embedding_probe_interval_s=60`, `embedding_cache_size=256`. *Under BigTiny these
  are overridden at startup to EmbeddingGemma's live width and a `litert:` tag.*
- **Chunking / clustering**: `chunk_chars_min=512`, `chunk_chars_max=1024`,
  `cluster_similarity_threshold=0.92`, `semantic_dedup_threshold=0.92`.
- **Decay τ**: `tau_static_days=365`, `tau_transient_days=7`, `tau_pre_days=7`,
  `tau_post_hours=48`.
- **Disputes / probe**: `dispute_strength_floor=0.5`, `conflict_probe_neighbors=5`,
  `conflict_probe_similarity_min=0.55` (window upper bound is a fixed `0.98`).
- **Reliability / promotion**: `promotion_min_reliability=0.6`,
  `reinforcement_promote_threshold=5`, `reinforcement_count_max=5`,
  `reliability_drift_up=0.15`, `reliability_drift_down=0.25`,
  `reliability_band=0.2`, `archive_retention_days=90`.
- **Retrieval**: `retrieval_seed_chunks=8`, `retrieval_min_seeds=3`,
  `retrieval_depth_cap=3`, `activation_prune_threshold=0.05`,
  `activation_hop_decay=0.5`, `metaboost_cap=3.0`, `correlation_lambda=0.05`,
  MetaBoost addends (urgency 1.0/0.4/0.1, importance 0.8/0.3/0.1, unknown-triage
  premium 0.5).
- **Rendering caps**: `output_token_cap=1500` (full recall / MCP lookup payload),
  `injection_token_cap=500` (compact per-turn summary+index injected into context).
- **Extraction** (`Config.extraction`, used by the standalone/test path only — the
  BigTiny plugin injects `SummarizerChain` and ignores these): `provider_url`,
  `model="qwen3:4b"`, `timeout_s=12`, `retry_backoff_s=60`, `max_concurrent=1`,
  `batch_size=5`.
- **Maintenance**: `maintenance_tick_s=60`, `maintenance_heavy_interval_hours=24`.

Rule: if a number appears in a code path, it is a `Config` field. Adding a knob
means adding a boundary test (`tests/config.rs` pins every default) and updating
the sample YAML.

---

## 5. Embedding layer (`src/embed/`)

- **`hashing.rs`** — the deterministic fallback. In-house MurmurHash3 x86-32
  (`mmh3_32`, seeds 2026/2027), signed feature-hashing of `[a-z0-9]+` tokens into
  a unit-normalized vector. Always tagged `__lexical_hash__`. Same text → same
  vector, forever; this is what makes tests reproducible offline.
- **`project.rs`** — `project(raw, dim)`: wrap-add fold when the raw vector is
  wider than `dim`, pad when narrower, then L2-normalize. Used to fit a model's
  native width to the configured dim.
- **`provider.rs`** — `OllamaEmbedder`: POSTs `/api/embeddings`, with a **circuit
  breaker** (after a failure it fast-falls-back to hashing without paying the
  timeout again until `probe_interval` elapses) and an LRU text→(vector,tag)
  cache. On any failure or empty response it returns a `__lexical_hash__` vector.
  `embed_fresh` bypasses the cache for the `reembed_stale` recovery pass.

---

## 6. Ingestion pipeline (`src/learn/`)

`Engine::ingest(&IngestInput) -> IngestOutcome` (`learn/mod.rs`) runs Stages 0–3
**synchronously** in one transaction; Stage 4 (`learn/extraction.rs`) is
**asynchronous**, driven by `drain_extraction` (and by the maintenance sweep in
production).

`IngestInput { content, source_type, source_name, source_entity, captured_at, intent }`.

- **Stage 0 — PII gate** (`privacy.rs`). Scanned *before any write to disk*.
  Deterministic detectors (email, SSN, credit card via Luhn, API key, phone,
  address) plus an optional LLM classifier. A hit rejects the document. Audit
  records the PII **category only**.
- **Stage 1 — Document hash.** SHA-256 of the whole payload → `document_hash`. If
  already seen, or if a matching tombstone exists (§13), abort — the document is
  not re-ingested/re-learned.
- **Stage 2 — Parse & chunk.** Deterministic windowing into 512–1024-char chunks
  with ~50% overlap (`chunk_text`). Determinism matters: identical text must yield
  identical chunks so the content hashes are stable.
- **Stage 3 — Per chunk.** Normalize text (`text::normalize_text`: NFC,
  whitespace-collapse, trailing-punctuation strip, **case-preserving**) →
  `content_hash` (SHA-256 of normalized text). Skip if the hash is already stored
  (dedup). Embed; resolve the **provenance cluster** (`resolve_cluster`: a fresh
  chunk joins the best same-space cluster only at CosSim ≥
  `cluster_similarity_threshold`, else starts its own). Seed the source's
  reliability (§12). Insert the chunk with `status='active'`,
  `extraction_status='pending'`, `decay_class='transient'`, `anchor_at=captured_at`.
  The whole document — its `documents` row, then each chunk row + vector row, then
  the sealed `chunk_count` — is one transaction; a half-written document is never
  visible.

Citations are **deterministic**, never LLM-generated:
`cluster_citation = "{source_type}: {source_name} / {YYYY-MM-DD}"` and
`provenance_cluster_id` is a dateless slug of `"{source_type}: {source_name}"`
(one cluster per source).

A freshly ingested chunk is `pending` and therefore **invisible to recall** until
Stage 4 extracts its propositions (retrieval joins chunks→propositions on active
props, and a pending chunk has none).

### Stage 4 — Extraction (`learn/extraction.rs`)

`Engine::drain_extraction(now) -> DrainOutcome` drains the ready `pending` queue
oldest-first, bounded by `extraction.batch_size`, honoring the per-chunk retry
backoff (`retry_backoff_s`), one LLM completion at a time (global semaphore),
under the `timeout_s` budget. Per chunk:

1. **Suppression check** — a suppressed chunk (§13) is drained to `done` with no
   LLM call and no writes.
2. **Local Neighborhood Probe** (`probe_neighbors`) — a *bounded* vector search
   over active chunks (never a full-DB scan), windowed to
   `[conflict_probe_similarity_min, 0.98]`, excluding self, **cross-source
   neighbors first**. These are the only valid contradiction targets.
3. **`StructuredChat` call** with `extraction_schema()` — returns
   `{ propositions: [...], disputes: [...] }`. Each proposition has `claim`,
   `importance`, `urgency`, `decay_class`, `urgency_expires_at`.
4. **Validate** each claim (`verify_claim`): normalize; drop if empty; **drop if a
   tombstone matches** the normalized text (§13); a `deadline` claim with an
   unparseable expiry is dropped (never wedges the chunk). Deterministic
   `node_id = "p_" + sha256(chunk_id | normalized_claim)[..24]` so a re-extraction
   dedups.
5. **Disputes** — only against probe neighbors, only at
   `strength ≥ dispute_strength_floor`, written as canonical-ordered
   `disputed_edges` with a deterministic edge id; re-opening the same pair is an
   idempotent no-op.
6. **Write, one transaction**: disputes → the chunk's decay profile
   (`resolve_decay_profile`: earliest valid deadline wins, else static, else
   transient) → propositions with derived confidence → support links →
   `finish_extraction` (sets `done` + decay profile atomically) → advance the
   watermark → **recompute confidence** for every proposition touched by a new
   edge (an opened edge changes both endpoints' weights).

A failed pass leaves the chunk `pending` with `extraction_error_at=now`; it
retries only after `retry_backoff_s`.

`DrainOutcome { attempted, succeeded, failed, skipped_suppressed, propositions_written, disputes_opened }`.

---

## 7. Core math (`src/core.rs`)

Pure, I/O-free, deterministic functions. Phases 6–8 compose these around their
store queries. Timestamps arrive as ISO-8601 UTC `…Z` strings, parsed by
`parse_utc`. Every function is total on its domain (unknown class / bad timestamp
→ `None` or `0.0`, never a panic).

- **Decay** `decay(class, anchor, t_expire, now, cfg) ∈ [0,1]`:
  - `static`/`transient`: `e^-(now - anchor)/τ` (τ = 365d / 7d).
  - `deadline`, piecewise around `T_expire`: before it,
    `0.5 + 0.5·e^-(T_expire - now)/τ_pre` (a salience ramp that **rises to 1.0 at
    the deadline**, τ_pre = 7d); after it, `e^-(now - T_expire)/τ_post`
    (incident window, τ_post = 48h).
- **`earliest_deadline(expiries)`** — `T_expire(P) = min` over active chunks.
- **`deadline_elapsed` / `urgency_is_unknown`** — post-deadline window fully
  elapsed (`now > T_expire + τ_post`); the point where `urgency: high → unknown`.
- **`should_archive_chunk`** — archive when decay `<` prune threshold, or (deadline
  class) once the incident window elapsed.
- **`dispute_strength(max_open_edge)`** — max strength of a chunk's open disputed
  edges, else 0.
- **`effective_weight(reliability, dispute_strength)`** =
  `reliability × (1 − dispute_strength)` — the **only** channel by which a
  contradiction lowers confidence.
- **`source_weight(λ, effective_decays)`** = `max(w_eff·decay) + λ·ln(n)`, bounded
  to `1 − 10⁻⁶`. **Max, not sum**: near-duplicate chunks of one source collapse to
  their best; the `λ·ln(n)` term is a small diminishing-returns bonus for repeated
  same-source evidence.
- **`confidence(λ, sources)`** = `1 − ∏_S (1 − W_S)` over **distinct
  `source_entity`**. Two clusters from the same origin fold into one `W_S` and can
  never multiply confidence.
- **`seed_activation(cosines)`** = `max`, clamped `[0,1]` (Pass 1 seed).
- **`hop(activation, edge_weight, γ)`** = `activation · edge_weight · γ` (Pass 2).
- **`should_prune(activation, δ)`** — below the floor.
- **`metaboost(urgency, importance, cfg)`** =
  `min(1 + urgency_boost + importance_boost, cap)`; `importance` outside the four
  levels takes the unknown-triage premium.
- **`priority(activation, boost, cap)`** = `activation · boost` clamped `[0, cap]`
  (multiplicative, not additive).
- **`unsupported_transition(importance)`** — `high`/`unknown` →
  `UNSUPPORTED_ARCHIVE`, else `DELETE`.
- **`can_promote(...)`** — all four gates: `reinforcement_count ≥ threshold`, no
  open dispute, `reliability ≥ floor`, **and** independent corroboration (≥ 2
  distinct `source_entity` or `controlled_test`). Frequency alone never promotes.

---

## 8. Retrieval (`src/recall/`)

The read path. `Engine::recall`, `search_index`, and `render_item` all share one
two-pass core, `ranked_items(query)`:

**Pass 1 — seed.** Embed the query; `VectorIndex::search` the top
`retrieval_seed_chunks` **chunks**; map each retrieved chunk to the active
propositions it supports; each proposition's seed activation = `max CosSim` over
its retrieved supporting chunks (clamped). If fewer than `retrieval_min_seeds`
propositions seed, widen the search once over the whole active pool. Pending
chunks contribute nothing (no active props → invisible).

**Pass 2 — spread.** Bounded spreading activation over `proposition_edges`: depth
≤ `retrieval_depth_cap`, each hop `= hop(act, edge.weight, γ)`, paths below
`activation_prune_threshold` pruned, keeping each node's best activation.

**Rank.** For each activated proposition, `priority = priority(activation,
metaboost(urgency, importance), cap)`; sort descending, ties broken by `item_id`
so repeated calls are byte-stable. Propositions with no active support are
dropped. `item_id` **is the proposition's `node_id`** — a stable handle the model
looks items up by.

Three renderings off the ranked set:

- **`recall(query) -> Option<String>`** — the per-turn injection. A short summary
  line plus a one-line-per-item index (`- [item_id] claim — conf 0.NN —
  citation`), capped at `injection_token_cap` (~500 tokens; the accounting rounds
  *up* so the cap is never exceeded). Returns `None` when nothing seeds → zero
  prompt delta. **Side effect:** writes the grounding/reinforcement outbox rows for
  the indexed items' chunks (below), in one transaction.
- **`search_index(query, offset, limit) -> Vec<IndexEntry>`** — the same ranked
  index, paginated; backs `memorabilia_search`. **Read-only** (never grounds).
- **`render_item(item_id, offset, limit) -> Option<ItemView>`** — one item's full
  claim + supporting chunk text + citations, windowed over supporting chunks,
  byte-budgeted so the id/metadata always survive truncation; backs
  `memorabilia_read_item`. Returns `None` for an unknown or non-active id (a
  deleted/archived item never resurfaces).

**Grounding & reinforcement outbox** (`write_grounding`, principles below). Only
the per-turn injection writes to the outbox — lookups are side-effect-free, so one
query event increments a chunk at most once. For each chunk backing an indexed
item, one deduplicated row is enqueued: `grounded = true` always;
`reinforced = true` only when the chunk has **no open dispute**. Deferred, not
applied here — the maintenance drain applies the counters (§9). This is what
enforces two invariants: every rendered chunk is grounded (audit) exactly once per
query event, and a disputed chunk is grounded but **never reinforced** (the system
must not amplify evidence it flags as contradictory).

---

## 9. Maintenance (`src/maintenance.rs`)

`Engine::maintenance_tick(now) -> MaintenanceOutcome`, driven on a cadence by the
plugin's background sweep. `now` is the caller's timestamp (never the wall clock in
tests). Each phase logs-and-swallows its own errors, so one failing phase never
aborts the tick.

**Every tick:**

1. **Outbox drain** — apply the enqueued grounding/reinforcement counters. A
   reinforced non-deadline chunk resets its **anchor** (`anchor_at = now`, not τ);
   `reinforcement_count` is capped at `reinforcement_count_max`. Delete the drained
   rows. `UNIQUE(chunk_id, query_event_id)` makes replay idempotent.
2. **Decay resolution** — for each active chunk, compute decay; if it should
   archive, archive it (remove its vector, best-effort), **close its open disputed
   edges** (archived materials drop out of dispute — recompute the other
   endpoints' confidence), then for each proposition it supported: if active
   support is now zero, apply `unsupported_transition` (archive if high/unknown,
   else delete); otherwise recompute confidence.
3. **Retention hard-delete** — archived chunks and `UNSUPPORTED_ARCHIVE`
   propositions older than `archive_retention_days` are hard-deleted (FK cascades
   clear links/edges/outbox; the vector row is dropped through the seam first).

**Behind the 24-hour gate** (`last_maintenance_at`):

4. **`reembed_stale`** — chunks whose vectors are still `__lexical_hash__` are
   re-embedded once the semantic embedder recovers; a vector that still comes back
   hashed is left as is.
5. **Promotion** — transient chunks that pass all four `can_promote` gates become
   `static`.

**Not implemented (documented gaps):** reliability *drift* (needs a
dispute-outcome ledger that does not exist yet) and the `RHYMES_WITH` pool
(remaining Phase-9 work). Both are intentionally out of the current tick.

---

## 10. Privacy & the forget ladder (`src/privacy.rs`)

- **Stage 0 PII gate** rejects PII before any disk write; `scan_pii` runs
  deterministic per-category detectors, optionally backed by a `PiiClassifier`.
- **`Engine::forget(phrase, reason)`** — the deletion ladder, three reasons:
  - `wrong` — permanent **suppression** + a text-hash **tombstone** so the text is
    never relearned.
  - `outdated` — time-bounded suppression (`outdated_suppression_days`, default
    90); the content may be relearned after it expires.
  - `private` — **hard delete** + tombstone. The regression guard: a `private`
    delete must never surface the text again.
- **Tombstones** are recorded only when the deleted text clears
  `tombstone_min_chars` **or** carries a high-entropy token (a digit run ≥
  `tombstone_digit_run`, or a PII pattern) — short generic phrases are deleted but
  not tombstoned. They are matched at ingestion (`content_hash`/`document_hash`)
  and at extraction (normalized proposition text, **case-sensitive** because
  `normalize_text` preserves case).
- **Suppressions** hide a specific chunk from extraction (drained to `done` with no
  props); an `outdated` one expires.

Audit rows never contain PII values — category only.

---

## 11. Reliability (`src/reliability.rs`, `src/store/registry.rs`)

`source_reliability` is **never hand-coded**. On first sight of a `source_entity`
it is seeded once: `clamp(tier_prior × intent_factor)`, where the tier comes from
data files (`data/reliability.yaml`: a tier table + reputable-domain set — *data,
not user config*) and the intent factor from the attachment intent. It then drifts
from outcomes (corroboration up `+0.15·(1−r)`, dispute-loss down `−0.25·r`),
bounded within `±reliability_band` of the tier prior. (The drift step is specified
and the seed is implemented; the automatic drift *trigger* is the documented gap in
§9.) Re-seeding is a no-op so accumulated drift is never erased.

---

## 12. The MCP lookup server (`src/mcp/`)

An `rmcp` (=2.2.0) in-process server, `MemorabiliaServer::new(Arc<Engine>)`,
served over a duplex stream by `serve_in_process`. It follows the **kitty-tools
document-reading pattern**: an injected index gives the model stable ids, and these
tools resolve them on demand — `item_id → memorabilia_read_item / memorabilia_search`,
exactly as kitty-tools does `document_id → doc_read_chunk / doc_search`.

Two read-only tools (grounding/reinforcement happens only in the per-turn
injection, so a lookup never double-counts):

- **`memorabilia_search { query, offset?, limit? }`** — ranked items, each
  `{ item_id, claim, confidence, disputed, citation }`, offset-paginated
  (default 25, cap 200).
- **`memorabilia_read_item { item_id, offset?, limit? }`** — one item's full
  claim + supporting chunks `{ citation, content }` + confidence/importance/
  urgency/disputed, windowed over supporting chunks (default 10, cap 100).

Conventions mirror kitty-tools: a uniform envelope
`{ status, truncated, data, message?, metadata? }` on success and
`{ status:"error", error_code, message, hint }` on failure; `SCREAMING_SNAKE`
domain-prefixed error codes (`MEMORABILIA_ITEM_NOT_FOUND`,
`MEMORABILIA_BAD_REQUEST`); zero-based `offset`/`next_offset`/`has_more`
pagination; a response byte budget that guarantees the `item_id`/`next_offset`
survive truncation; and load-bearing tool names.

---

## 13. BigTiny V2 integration (the production host)

BigTiny V2 (`D:\Coding Projects\Kitty\Kitty\BigTinyV2`) is a shared, machine-wide
LLM-orchestration daemon. Its **plugins are compiled-in Rust**, hooking the agent
loop; the pre-existing `pathway` plugin is built on the `adaptive-pathway` crate,
which is the reference Memorabilia mirrors. Memorabilia is wired in as a **second
compiled-in plugin**, exactly parallel to pathway. All daemon code below is under
`BigTinyV2/daemon/src/`.

- **Path dep** in `daemon/Cargo.toml`; **`MEMORABILIA` plugin const** in
  `storage/app_plugins.rs`; **`MemorabiliaConfig`** in `config.rs`
  (`enabled` default false, `db_name`, `learn_every_n=4`, `sweep_interval_s=60`).
- **`MemorabiliaHost`** (`plugins/memorabilia_host.rs`) — parallel to `PluginHost`.
  Opens one `Engine` per app at `apps/<app_id>/memorabilia.db` via
  `Engine::open_at`, lazily, and spawns a background maintenance sweep per live
  instance (bounded `maintenance_tick` on `sweep_interval_s`, stopped on close).
  Two adapters supply the seams from resources the daemon already owns:
  - **`SharedSemanticEmbedder`** wraps the daemon's shared
    `Arc<dyn SemanticEmbedder>` (the **same** loaded EmbeddingGemma model pathway
    uses — never a second copy) into Memorabilia's `Embedder`, projecting to the
    configured dim and falling back to the hash embedder on `None`. The dim is
    read from a live probe embed at startup; the space tag is the shared model's
    identity (`litert:<stem>`).
  - **`SummarizerChatAdapter`** wraps the daemon's `SummarizerChain` (which already
    implements the *identical* `StructuredChat` signature) into Memorabilia's
    `StructuredChat` — the Gemma E2B → configured-provider extraction/maintenance
    path.
- **Wiring**: `AppState.memorabilia` (`routes/mod.rs`); constructed in `lib.rs`
  (probes the embedder's dim, shares the embedder + summarizer) and closed on
  shutdown; `mcp.attach_memorabilia(...)`.
- **Enable/disable API** (`routes/plugins.rs`): `memorabilia` is in
  `KNOWN_PLUGINS`; `GET/PUT/DELETE /api/apps/me/plugins/memorabilia` dispatch to
  the right host. Off by default → an app opts in with
  `PUT /api/apps/me/plugins/memorabilia {enabled:true}`.
- **MCP tools in-process** (`mcp/builtin.rs` `"memorabilia"` arm +
  `BUILTIN_SERVERS`; `mcp/manager.rs` `memorabilia_engine_for` resolves the calling
  app's engine). The `every_advertised_builtin_actually_connects` conformance test
  covers it.
- **Agent-loop hooks** (`agent/loop_.rs`, threaded through `agent/mod.rs`):
  - **Turn-start injection** — `memorabilia_recall(session_id, user_message)`
    resolves the app engine and calls `Engine::recall`, under the same time budget
    as pathway. Its compact summary+index is **merged into the existing `ap_hints`
    tail `system`-block channel** (both plugins can be on) so the stable prompt
    head stays byte-identical for prefix caching.
  - **Turn-end learn** — every `learn_every_n` exchanges, ingest the session's
    latest user+assistant exchange as a `Conversation` document
    (`source_entity="session:<id>"`) and drain a slice of Stage 4 so it becomes
    recallable. Fire-and-forget; ingest is idempotent by content hash.

**Remaining host step (not done):** the desktop Kitty app
(`src-tauri/src/bigtiny/mcp.rs::ensure_builtin_servers`) must register an
`in_process` MCP server row named `"memorabilia"` (mirroring the `"pathway"` row)
and add it to `REGISTERED_BUILTINS`, gated by a new Kitty setting, for the lookup
tools to auto-advertise in the desktop app. The context-injection hook and the
enable/disable API already work without it.

---

## 14. Invariants the tests pin (don't break these)

These are asserted across `tests/` and are the contract of the system:

- A `pending` chunk is invisible to recall until extraction completes.
- Decay/lifecycle live on chunks; a proposition with zero active support leaves
  the graph (`high`/`unknown` → archive, else delete).
- Same-`source_entity` clusters do **not** multiply confidence (distinct-source
  aggregation, `max` per source).
- Opening a `DISPUTED` edge lowers the victim's confidence; archiving the disputer
  closes the edge and restores it.
- A disputed chunk increments `grounding_count` but **not** `reinforcement_count`.
- `Priority` ordering is deterministic across repeated calls; a higher-cosine seed
  outranks a lower one at depth 0.
- The injection block never exceeds `injection_token_cap`; the full payload never
  exceeds `output_token_cap`.
- Every index `item_id` resolves through `render_item`; an unknown/archived id
  never resurfaces text.
- A `private` delete never surfaces the deleted text again.
- A `__lexical_hash__` vector never cosine-compares against a semantic vector.
- Contradiction candidates come only from the probe window; short generic phrases
  are deleted but not tombstoned.
- The outbox is at-least-once + idempotent; one query event increments a chunk
  once.
- `recall` returns `None` when there is nothing to say — byte-identical to
  no-memory behavior.

Testing discipline: in-process integration tests, mocked seams (`MockChat` +
deterministic hash embedder), in-memory SQLite, `Config`-injected thresholds, no
live network, no exact-prose assertions. New behavior without a test is
incomplete; a new `Config` knob needs at least one boundary test.

---

## 15. Source map (where to look)

```
migrations/            001 settings · 002 schema · 003 extraction retry
config.yaml            sample config (every knob, = defaults)
src/
  lib.rs               module list
  config.rs            Config + ExtractionConfig, YAML loader, all defaults
  error.rs             Error/Result
  traits.rs            StructuredChat / Embedder / VectorIndex + MockChat + MemoryVectorIndex
  text.rs              normalize_text, sha256_hex, slug, citation_date  (pure)
  core.rs              decay/confidence/activation/priority/lifecycle    (pure)
  engine.rs            Engine struct, new(), open_at()
  embed/               hashing (fallback) · project · provider (Ollama + breaker + cache)
  store/               db (pool, txn, watermark) · chunks · propositions · edges ·
                       documents · registry · outbox · tombstones · audit · vectors
  learn/               mod (ingest Stages 0–3, chunking, clustering) · extraction (Stage 4)
  recall/              two-pass retrieval, index/item/search rendering, outbox writes
  maintenance.rs       maintenance_tick (per-tick + 24h heavy pass)
  privacy.rs           PII gate, forget ladder, tombstone guard
  reliability.rs       tier table + seed/prior, drift primitive
  mcp/                 MemorabiliaServer (memorabilia_search / memorabilia_read_item)
tests/                 config core embed reliability store learn extract privacy
                       recall maintenance mcp noop   (invariant-pinning integration tests)
```

BigTiny-side wiring (in `BigTinyV2/daemon/src/`): `plugins/memorabilia_host.rs`,
`config.rs` (`MemorabiliaConfig`), `storage/app_plugins.rs` (`MEMORABILIA`),
`routes/plugins.rs`, `routes/mod.rs` (`AppState`), `lib.rs` (construction),
`mcp/builtin.rs` + `mcp/manager.rs` (in-process tools), `agent/loop_.rs` +
`agent/mod.rs` (recall + learn hooks).

---

## 16. Status & known gaps

- **Done and tested:** the full engine (ingest → extract → recall → maintain),
  privacy, the MCP lookup server, and the BigTiny plugin (host, adapters,
  injection + learn hooks, in-process tools, enable/disable). The crate's test
  suite and the daemon's test suite both pass.
- **Deferred:** reliability drift (needs a dispute-outcome ledger); the
  `RHYMES_WITH` pool + revalidation; the desktop `src-tauri` MCP-row registration
  and its Kitty setting (§13). None block the core behavior.

When you change a design decision, change the code and this file together, and add
or update the test that pins the new behavior.
