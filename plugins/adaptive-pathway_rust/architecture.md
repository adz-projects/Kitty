# architecture.md — adaptive-pathway_rust

An **in-process** behavioral-memory engine linked into the BigTiny daemon
(`bigtiny_rust`). It models who the user is, what they care about, and how the
assistant should adapt. It owns its own `pathway.db`, does semantic recall and
learning in-process, and exposes only two model-chosen write tools over MCP
(`record` / `forget`). The dependency direction is inverted: the crate depends
only on the `StructuredChat` trait, which the daemon implements over its
`SummarizerClient` LLM client.

---

## Combined flowchart

```
                    ┌────────────────────────────────────────────────────────┐
                    │  ENGINE STATE — PathwayEngine (one Arc shared w/ host)  │
                    │    db  = pathway.db (own SQLite, WAL, migrations 001–3) │
                    │    embed = EmbeddingProvider (Ollama /api/embeddings    │
                    │            → deterministic hash fallback; text LRU cache│
                    │    paused_override = DashMap + conversation_state.paused│
                    │    learn_locks = per-session DashMap<Mutex>             │
                    │    chat_slot = global 1-permit Semaphore                │
                    └────────────────────────────────────────────────────────┘

   ─────────────── READ PATH — recall (turn start, in AgentLoop.pathway_recall)
   │
   │  is_paused(session_id)?  ──true─►  None   (zero prompt delta, cache-safe)
   │
   ▼  list recall candidates
   │   (context + identity beliefs: all sessions; conversation: THIS session)
   ▼  embed the user message  (best-effort, 1500 ms budget; timeout → empty vec)
   ▼  select_beliefs_relevant(candidates, query, domain)
   │     score = effective_weight × (0.5 + 0.5 × cosine(query, belief))
   │     cap pool at MAX_CANDIDATES = 64
   │     drop suppressed-as-zero (weight 0.0)
   │     greedy DPP over capped set → pick ≤ MAX_BELIEFS = 6 (diversity)
   ▼  render_knows(selected) → sort (weight desc, then id asc) → "- text" lines
   ▼  return "[What I know about you]\n...lines\n\n" + FOOTER
   ▼  injected as ap_hints into the prompt TAIL region (never the stable head,
   │   so KV-prefix caching stays byte-identical — deterministic output)
   │
   │  effective_weight = (base: tested?conf : conf×0.625)      (untested ×0.625)
   │                    × domain_match (in-domain 1.0 / cross 0.35)
   │                    × decay_factor = exp(−Δdays / half-life)
   │                      (conversation 1 d · context 45 d · identity 365 d,
   │                       days quantized to whole)
   │                    × contradiction factor (0.5 if contradict_count > 0)
   │                    pinned ⇒ floor at 0.8

   ─────────────── WRITE / LEARN — extract_and_record (all three triggers)
   │   triggers: TurnEnd (turn) · IdleClose (background sweep) · Compaction
   ▼
   ① acquire per-session learn lock (CAS-like mutex; guard released on drop)
   ② paused? ──► skip
   ③ watermark = last_learned_rowid(session);  through ≤ watermark ──► skip
       (forward-only watermark = the double-count guard across all triggers)
   ④ build chunk: given_chunk, else host::read_unlearned_chunk(host db,
      watermark..through) — role≠'system', keep NEWEST ≤12 000 chars, time-order
       empty chunk ──► advance watermark & skip
   ⑤ render top-20 known beliefs (by effective weight) for the prompt
   ⑥ acquire the global chat permit (one structured_chat at a time)
   ⑦ LLM structured_chat(build_extraction_prompt(known,chunk),
       extraction_schema) → observations[] + corrections[] (+ tone/topics)
       schema forbids identity layer; empty list is valid
   ⑧ truncate observations to ≤ 5, IN RUST
   ⑨ per observation → route_observation(...)
   ⑩ per correction → forget_by_text(correction, reason=Wrong)
   ⑪ advance watermark = max(watermark, through_rowid); audit log
       errors → tracing::warn only; never fail the turn

   ─────────────── route_observation(statement, embedding, prov, layer, ...)
   ▼
   ① permanent tombstone for text_hash(statement)? ──► skip (never relearn)
   ② merge candidates = same layer w/ THIS session scoping for conversations,
      else every belief in that layer
   ③ best cosine ≥ MERGE_COSINE (0.86)?
         YES → merge into existing:  confidence = reinforce_toward(bound,+) 
                provenance = max(prov today)   support_count+1   distinct_session
                counting, last_confirmed_at=now, re-parent observation
          NO  → new belief: confidence = proven_initial (single 0.15 …
                correction 0.75), tested = prov.is_tested()
   ④ insert observation row (audit trail survives belief pruning)
   ⑤ contradicts field → best_text_match(text) → insert_contradiction(open)
      (best-effort; never fatal)

   ─────────────── background::run — 1 loop per 60s tick (watched shutdown)
   ▼
   idle_sweep: host session ids idle ≥15 min OR active ≥30 min (SQL
   datetime('now'), ≤ IDLE_SWEEP_BATCH=3 per tick) → per id:
        paused? skip
        extract_and_record(IdleClose)  (same watermark discipline)
        consolidate_session(id)
   then run maintenance each tick
   (tick uses MissedTickBehavior::Skip; select! on 60s tick vs shutdown_rx)

   ─────────────── consolidate_session(session_id)
   ▼
   ① load THIS session's conversation-layer beliefs only
   ② drop weak: confidence < 0.25 OR (provenance==single_observation AND
      support ≤ 1)              (observations kept as audit trail)
   ③ each survivor: match cosine ≥ 0.86 to a CONTEXT belief
          → merge (reinforce confidence, sum support, add distinct session,
            re-parent its observations, delete source belief)
      else promote to context (layer=context, session_id=None)
   ④ promote context → identity when ALL 4 gates pass:
          support ≥ 3  AND  distinct_sessions ≥ 2  AND
          (provenance ∈ {direct_statement, controlled_test, correction} OR tested)
          AND  confidence ≥ 0.65  →  consolidated_at=now
   ⑤ contradiction pass over open contradictions
   ⑥ audit log

   ─────────────── maintenance::run_maintenance (per 60s tick, gated nightly)
   ▼
   every tick: prune expired (non-permanent) suppressions
   if 24h cold since last_maintenance_at (self-nominated persisted):
        foreach TESTED belief whose last_confirmed_at ≥ 30 days:
          confidence −= 0.15 (min 0);  last_confirmed_at = now
          re-enter assumption pipeline at state=scheduled if not already
   update last_maintenance_at + audit

   ─────────────── MCP tools (model-chosen writes, in-process PathwayServer)
   ▼
   record(observation, provenance?, domain_hint?):
       trim empty → "empty_observation"  ·  paused → "memory paused"
       embed  → route_observation(layer=Context, provenance parsed,
                                   domain_hint)    soft ok/error JSON
   forget(what, reason=Wrong|Outdated|Private):
       embed (REAL embedding for cosine match), plus last_recall_ids for recall
       forget_by_text → Wrong=permanent suppression + tombstone,
                         Outdated=90-day suppression, Private=hard delete
       returns exact dropped text (soft JSON: "nothing matched" fallback)
```

**Guarantees.** Every entry point is a "soft no-op" when the engine is absent,
paused, or empty (returns a skip, a `None`, or a benign message) — so when the
engine has nothing to say, the prompt is byte-identical and the model is none
the wiser. Learning, recall, and consolidation are all serialized at the right
granularity (one LLM completion at a time via the global semaphore; one learn
pass per session via the session lock; watermark prevents re-learning). Any
DB/LLM error inside is `tracing::warn`-logged and swallowed, never returned to
the caller as a failure.

---

## Technical description

1. **Positioning and dependency inversion.** `adaptive-pathway_rust` is a
standalone crate (not a workspace member of the desktop `src-tauri`, per that
repo's MSRV-isolation and feature-unification rules) that BigTiny links in as
its behavioral-memory engine. The engine inverts the expected dependency: it
does not know about LLM providers, routers, or the session scheduler. It
depends only on `traits::StructuredChat`, a two-method JSON-schema-constrained
completion interface, and the daemon implements that trait over its
`SummarizerClient`. This one seam lets the same engine run under tests
(`MockChat`), under the daemon, and under any future host. Everything durable
lives in the engine's own `pathway.db` — a separate SQLite file with its own
migration chain (WAL, `foreign_keys`, `busy_timeout`), sharing a directory with
the host's `big.db`.

2. **Three-layer belief model and the recall weight.** Beliefs are the atomic
unit of behavioral memory, arranged in three layers with per-layer decay:
**conversation** (1-day half-life, session-owned, "lives for the session"),
**context** (45-day, cross-session), and **identity** (365-day). Rather than
decaying stored floats, the engine recomputes an `effective_weight` on the fly
each call: the raw `base` (confidence, ×0.625 if untested — the untested
discount that makes 0.80 drop to 0.50) is multiplied by a domain
routing factor (1.0 in-domain / 0.35 for cross-domain, deliberately a routing
decision, never deletion), by an exponential decay that is quantized to whole
days so the top-6 ordering cannot flip mid-day and perturb the prompt cache, and
by 0.5 when the belief is contradicted, with a 0.8 floor when pinned.
Provenance (`correction` > `direct_statement` > `controlled_test` >
`inferred_pattern` > `single_observation`) assigns the initial confidence and
the reinforcement step; weak evidence structurally cannot carry a belief fast.

3. **Recall at turn start (read path).** Every turn the daemon calls
`pathway_recall`, which first gates on the session's pause state and then loads
its candidate beliefs: every context/identity belief plus exactly this
session's conversation-layer beliefs, so one session's transient memory never
leaks into another session's recall block. It embeds the current user message
under a 1.5s best-effort budget — a cold or down embedder degrades to the empty
vector (pure weight-driven selection) rather than a hard failure — then calls
`select_beliefs_relevant`. That function scores each candidate as
`effective_weight × (0.5 + 0.5·cosine(query, belief))`, caps the pool at 64 so
the DPP kernel stays bounded however large the store grows, filters
weight-zero (suppressed) beliefs, and greedily samples a determinantal point
process over the cap to pick ≤6 beliefs that are simultaneously strong and
diverse. The result is rendered stable sorting and injected into the prompt's
**tail** region as `[What I know about you]` plus a footer, leaving the stable
prefix byte-identical turn over turn — the KV-prefix cache guarantee.

4. **Learning, merging, and the shared extract pipeline.** Learning funnels
through one function, `learn::extract_and_record`, from three triggers: the
turn-end seam (every `learn_every_n` user exchanges), the background idle
sweep, and compaction. It takes a per-session lesson (a mutex) and a
**forward-only watermark** (`last_learned_rowid_sql` in `conversation_state`) —
the single guard that prevents the three triggers from ever double-learning the
same tail — then builds a chunk from the host's `big.db` (dropping system rows,
keeping the newest 12k chars, preserving chronological order) and prompts the
LLM against its top-20 current beliefs with a schema that explicitly forbids
the identity layer. The returned observations are truncated to five in Rust and
each routed via `route_observation`, which skips permanently-forgotten
statements (tombstone text-hash), merges into an existing belief when cosine ≥
0.86 (reinforcing confidence recompute, bumping support/distinct session
counts) or creates a fresh one from the provenance's initial confidence, and
finally records model-reported contradictions. Corrections become
`forget(wrong)`; after the batch the watermark is advanced and the pass is
audited. Every step short of the one LLM completion is pure SQLite and
in-memory work in a single transaction; every error is logged, never raised.

5. **Background loop, consolidation, and the nightly maintenance.** A single
`background::run` task ticks every 60s (missed ticks skipped, watch-channelled
shutdown) and runs an idle sweep: it queries the host's `big.db` for sessions
whose `updated_at` is idle ≥15 min or stale-active ≥30 min, caps each sweep at
3 sessions to avoid a restart storm, and for each — unless paused — runs the
`IdleClose` learn pass followed by `consolidate_session`. Consolidation
promotes conversation-layer beliefs: weak ones (confidence < 0.25, or a lone
`single_observation`) are pruned (observations survive as audit), survivors
merge into a cross-session context belief at cosine ≥ 0.86 or are elevated to
context directly, and context beliefs promote to durable identity facts only
when all four gates pass (support ≥ 3, distinct sessions ≥ 2, a tested
provenance class, confidence ≥ 0.65). The same tick runs `run_maintenance`:
expired suppressions are pruned every tick, and a 24-hour-gated nightly
pass applies the 30-day "preference re-evaluation" — stale *tested* beliefs
lose confidence (0.15) and re-enter the assumption pipeline at `scheduled`,
using a persisted `last_maintenance_at` so the cadence survives restarts.

6. **MCP write tools and the global discipline.** The engine exposes exactly
two write seams to the model via the in-process `record`/`forget` tools behind
a pause check. `record` simply embeds the observed statement and routes it as a
context-layer observation with the requested provenance. `forget` reads the
session's `last_recall_ids` as context, embeds the user's phrase (a real
embedding, since the lexical fallback would put it a different vector space),
and applies the reason-severity ladder: `wrong` makes permanence (suppression +
a text-hash tombstone so extraction can never relearn it), `outdated` is a
90-day suppression, `private` a hard delete. Both tools answer soft
`{"status":"ok",…}` JSON rather than model-facing errors. The whole system —
recall, learn, consolidate, forget — is idempotent when there is nothing to do,
serialized at the scheduler and learn-lock level, bounded in its DPP/embedding
costs, and airtight at every seam — the daemon never blocks on it, and an
LLM/database hiccup inside can neither break the turn nor perturb the prompt,
so correctness degrades gracefully instead of failing loudly.