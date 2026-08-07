\# Adaptive Pathway → Behavioral Memory (Rust rewrite)



\## Context



`plugins/adaptive-pathway/` is a \~5,800-line Python package that learns \*\*which tool the agent should pick next\*\*. It ships as two frozen PyInstaller binaries: an HTTP sidecar Kitty spawns and supervises, and a stateless stdio MCP proxy BigTiny spawns. Its core is a contextual-bandit stack — Thompson sampling, a bootstrap ensemble, DPP-diversified hint selection — surfaced to the user as tool-choice nudges.



That solves the wrong problem. Per `plugins/adaptive-pathway/behavioral-memory-plan.md`, the goal is now \*\*behavioral modeling\*\*: who is this user, what do they care about, and how should the assistant adapt? Edges become learned beliefs about the user; confidence becomes "how well-established is this belief"; novelty becomes "what haven't we discovered about them yet." The anti-sycophancy machinery shifts from hidden scoring bonuses to \*visible\* signals the user can correct.



This is a rewrite in Rust at `plugins/adaptive-pathway\_rust/`, linked \*\*in-process\*\* into the BigTiny daemon. Rust is straightforward here: the real dependency surface is `numpy` (only L2 norms, dot products, `argsort`/`argpartition`, one `np.outer` rank-1 downdate, one degree-1 `polyfit`), `sqlalchemy`/`aiosqlite`, `pyyaml`, `mmh3`, `fastapi`, `mcp`. The declared `hdbscan`/`bertopic`/`onnxruntime` are in the `full` extra and \*\*never imported\*\*. The only genuinely hard numerics — Cholesky and multivariate-normal sampling in `decision/thompson.py` — live in modules being deleted. `plugins/kitty-tools/` is a working precedent for a Rust plugin in this repo.



\## Decisions locked



| Decision | Choice |

|---|---|

| Scope | \*\*Full replacement.\*\* No tool selection, no `decide` hints, no ensemble/Thompson/bandit. |

| Process shape | \*\*In-process\*\*, kitty-tools style. Both AP binaries and Kitty's whole supervision path retire. |

| Learn cadence | \*\*Compaction piggyback + every-N-exchange at turn end\*\*, one shared extraction fn. |

| Session close | \*\*Idle-timeout sweep\*\* in a daemon background task. |

| Existing data | \*\*Fresh `pathway.db`.\*\* Old file untouched on disk for rollback. |

| UI | \*\*Full shape\*\*: belief browser + belief-flavored Graph Health + Domain Profiles, plus in-chat correction, export, incognito toggle. |

| MCP tools | \*\*`record` + `forget`.\*\* Two, not one. |

| Recall budget | \*\*6 beliefs, 350-token hard cap\*\*, every turn. |

| Self-doubt cadence | `\[What I know]` + `\[Worth testing]` \*\*every turn\*\*; `\[Where I'm unsure]` \*\*every 12 exchanges\*\*; `\[Check yourself]` \*\*on detected plateau only\*\*. |

| Extraction model | \*\*Reuse `SummarizerClient` as-is\*\* (`LFM2.5-1.2b`). No second model. |



\## Architecture



\*\*Reads are direct in-process calls. Writes the model chooses to make are MCP tools.\*\*



The read hook already exists. `adaptive\_decide()` (`plugins/bigtiny\_rust/src/agent/loop\_.rs:1086`) calls `decide` over MCP under a 3s timeout and parses a \*\*Python-repr\*\* payload via `crate::pyrepr::try\_parse`. All of that dies: with the engine linked in, `pathway\_recall()` calls `engine.recall()` directly — no JSON-RPC round trip, no pyrepr parse, no tool-registration gate.



That gate is actively wrong for this design. The plan doc says recall is \*"deterministic — happens every turn, no exceptions"\*; gating on `active\_tools.iter().any(|t| t.name == "decide")` means a slow MCP connect silently costs the user their memory with no signal. Replace with `Option<Arc<PathwayEngine>>` on `AgentLoop` — `None` means \*configured off\*, not \*race lost\*.



\*\*Injection stays in the tail.\*\* `agent/context/builder.rs` layer 7, header literal changes from `\[Adaptive Pathway hints]` to `\[What I know about you]`. The `None` → zero-prompt-delta property and the tail placement are the entire KV-prefix-cache contract, enforced by `build\_messages\_is\_byte\_identical\_across\_repeat\_calls\_with\_identical\_input`. Do not move it into the stable head; the block reads like persona content, which makes this a tempting and expensive mistake.



\*\*Shared engine: widen `builtin::connect`, do not use a `OnceLock`.\*\*



```rust

// plugins/bigtiny\_rust/src/mcp/builtin.rs

pub async fn connect(name: \&str, server\_id: String,

&#x20;                    pathway: Option<\&Arc<adaptive\_pathway::PathwayEngine>>)

&#x20;   -> Result<MCPServerClient, MCPServerError>

pub const BUILTIN\_SERVERS: \[\&str; 4] = \["kitty-tools", "kitty-web", "kitty-wasm", "pathway"];

```



`MCPManager` gains `pathway: Option<Arc<PathwayEngine>>` via a new `with\_pathway(pool, engine)` so existing `new(pool)` test sites don't change; `manager.rs:59-64` passes `self.pathway.as\_ref()`. A global would make `builtin.rs`'s `every\_advertised\_builtin\_actually\_connects` test order-dependent (one process, whoever initializes first wins), and the engine owns a SQLite pool + WAL handle + background task that must be droppable and reopenable against a different path. Cost of explicit passing: \~12 lines.



\*\*Circular dependency\*\*, resolved by trait inversion — AP defines, `bigtiny\_rust` implements (orphan rule permits: foreign trait, local type):



```rust

// adaptive\_pathway/src/traits.rs

\#\[async\_trait::async\_trait]

pub trait StructuredChat: Send + Sync {

&#x20;   async fn structured\_chat(\&self, messages: Vec<Value>, schema: \&Value) -> Result<Value, String>;

}

```



Reading `bigtiny.db` stays at arm's length: AP takes `\&SqlitePool` as a parameter and issues \*\*exactly two\*\* raw `sqlx::query` reads (messages after a rowid; idle session list), both isolated in `learn/host.rs` with a doc comment naming them the coupling seam. If that grows past two queries, the boundary is wrong.



\## The crate



`plugins/adaptive-pathway\_rust/`, package `adaptive\_pathway`. Standalone — \*\*not\*\* a workspace member of `src-tauri`, same rationale as `plugins/kitty-tools/Cargo.toml` (MSRV isolation, `panic="abort"` must not leak, feature unification). No `\[profile.release]` block: as a path dep of `bigtiny\_rust` it would be ignored dead config.



```

src/  lib.rs config.rs error.rs traits.rs engine.rs layers.rs domains.rs

&#x20;     embed/{mod,hashing,project}.rs      vector/{index,dpp,cms,ops}.rs

&#x20;     store/{beliefs,assumptions,contradictions,observations,domains,

&#x20;            suppressions,conversation,settings,audit}.rs

&#x20;     belief/{mod,provenance,lifecycle,contradiction,synthesis}.rs

&#x20;     recall/  learn/{mod,host}.rs  consolidate.rs maintenance.rs

&#x20;     background.rs  mcp/  bin/devtool.rs

migrations/  001\_init.sql  002\_belief\_fts.sql

```



\*\*Ported unchanged (the math doesn't move):\*\* `embeddings.py` → `embed/` (Ollama `/api/embeddings`, mmh3 signed-hashing fallback, wrap-add projection to 384 dims, LRU, probe backoff); `storage/vec.py` → `vector/index.rs`; `decision/diversity.py` → `vector/dpp.rs` (44 lines, greedy MAP + rank-1 downdate); `decision/novelty.py` → `vector/cms.rs`; `learning/ttl.py` → `store/suppressions.rs`.



\*\*Deleted:\*\* `decision/{ensemble,thompson,info\_gain,paradigm\_challenge,selector,blending,in\_session}.py` and `engine.py`'s decision loop.



\*\*One `\[\[bin]]`, unshipped:\*\* `adaptive-pathway-devtool` with `recall` / `extract` / `dump` / `consolidate` / `serve-stdio`. Deliberately \*not\* in `plugins/build.py` or `externalBin`. It pays for itself immediately when tuning the extraction prompt.



\## Schema



\*\*Own file, `pathway.db`, in the same directory as `bigtiny.db`, with its own `sqlx::migrate!` chain.\*\* Rollback is a config flip; export is a file copy; delete-everything is a file delete; and the write burst from consolidation never contends with the chat hot path on one WAL writer. Sharing `bigtiny.db` would put AP migrations downstream of `bootstrap\_legacy\_python\_schema`, risking daemon startup on every schema bump. PRAGMAs match the daemon: WAL, `synchronous=NORMAL`, `foreign\_keys=ON`, `busy\_timeout=5000`.



Core tables: `beliefs` (text, embedding BLOB, confidence, provenance, `layer ∈ identity|context|conversation`, `tested`, domain, tier, support\_count, distinct\_sessions, contradict\_count, pinned, last\_confirmed\_at), `assumptions`, `contradictions`, `observations`, `domains`, `suppressions`, `conversation\_state`, `forget\_tombstones`, `audit\_log`, `novelty\_tables`, `synthesis\_log`, `app\_settings`, plus a `beliefs\_fts` virtual table.



Dropped vs. the plan doc's table: `nodes` (embeddings live inline), everything ensemble/action/feedback/override/telemetry. Added: `forget\_tombstones`, `audit\_log`.



\## The two learn seams



One shared function, two callers, one watermark.



```rust

pub async fn extract\_and\_record<S: StructuredChat>(

&#x20;   engine: \&PathwayEngine, host\_pool: \&SqlitePool, chat: \&S,

&#x20;   req: LearnRequest<'\_>, trigger: LearnTrigger,

) -> Result<LearnOutcome, PathwayError>;

```



Sequence: paused → skip → \*\*CAS the per-session learn lock\*\* (mirroring `sessions::try\_acquire\_compaction\_lock`, stale reclaim at `2 × timeout\_s`) → read `last\_learned\_rowid`; `through\_rowid <= last\_learned\_rowid` → skip (\*\*this is the double-count guard\*\*) → build chunk (given, or read `(last\_learned, through]` from `host\_pool`, drop `role='system'`, tool-mask, truncate to 12000 chars keeping newest) → render KNOWN BELIEFS (top 20 by effective weight) → \*\*acquire global 1-permit semaphore\*\* → `structured\_chat` → truncate to 5 observations \*\*in Rust\*\* → per observation embed/route/merge/upsert + assumption + contradiction in one transaction → process `corrections\[]` as `forget(wrong)` → `last\_learned\_rowid = MAX(last\_learned\_rowid, ?)` → release via guard.



Errors swallowed with `tracing::warn!`, matching `compaction.rs:933-939`. \*\*A failed extraction never fails a turn. Never `.await` a learn task on the turn path.\*\*



\*\*Schema discipline\*\* (copied from `MEMORY\_SLOTS\_SCHEMA`, `compaction.rs:28-60`): every field `required`; empty-string sentinels rather than nullable unions (grammar-constrained decoding is far more reliable when the grammar is total); and \*\*`layer` excludes `identity`\*\* — the extractor can never write a permanent fact, only consolidation promotes. That's a schema-level guard, so it can't be forgotten in code.



Fields: `observations\[]{statement, provenance, layer, domain, evidence, contradicts}`, `corrections\[]`, `tone`, `open\_topics\[]`. Prompt is exactly 2 messages mirroring `build\_summarizer\_prompt`, framed \*"learn about the user — not the task, not the assistant."\*



\*\*Seam A — compaction piggyback.\*\* `compaction.rs` stays free of AP knowledge; `CompactionResult` gains `folded\_chunk: Vec<Value>` and `folded\_through\_rowid: i64` — `run\_compaction\_inner` already builds both at lines 924/945 and discards them. Both call sites (`loop\_.rs:1058`, `routes/chat.rs:436`) spawn the AP pass \*from the result\*, so it runs after the memory-slot call returns, never concurrently.



\*\*Seam B — turn end, at `loop\_.rs:648`\*\*, immediately before `update\_session\_status(.., "idle")`. Not at the `break` on line 1009: line 648 is after `run\_tool\_loop` returns, so one call site covers \*every\* exit path (normal break, budget exhaustion, step ceiling, errors), and because `save\_messages` has already run it reads persisted rows — \*\*no `Vec<Value>` clone at all\*\*. Gate: `exchange\_count % learn\_every\_n == 0`, default 4. Follows `spawn\_record\_outcome`'s template exactly.



\*\*Ordering hazard worth a dedicated test:\*\* seam B learns through rowid 210, then compaction folds 100–200 and would set the watermark \*backwards\*. Guarded twice — the `<=` skip and the `MAX()` write.



\*\*Rule-based signals\*\* (plan open question 2) stay minimal and write \*\*no beliefs\*\*: reply length bucket / has-question / has-list (feeds plateau entropy), user length delta, lexical correction triggers, topic shift as cosine distance between consecutive user embeddings. They only drive anti-sycophancy signals and polarity hints. Rule-based signals inferring preferences directly is precisely the habituation-mistaken-for-preference failure this design exists to prevent.



\## Recall and anti-sycophancy



Effective weight, computed at recall, never stored:



```

base = if tested { confidence } else { confidence \* 0.625 }   // 0.80 → 0.50 exactly

w = base × domain\_match(1.0 | 0.35 cross-domain)

&#x20;     × exp(−Δdays\_quantized / half\_life\[layer])

&#x20;     × (contradicted ? 0.5 : 1.0) × (stale\_assumption ? 0.5 : 1.0)

w = if suppressed { 0.0 }; if pinned { w.max(0.8) }

half\_life: Identity 365d, Context 45d, Conversation 1d

```



\*\*That half-life table is the entire three-layer decay difference\*\* — no per-layer code paths. Cross-domain at 0.35 rather than the old bleed's \~0.036, because domain bleed becomes \*routing\*, not deletion. `Δdays` quantized to whole days so top-6 ordering can't flip mid-day and churn the block.



Provenance → initial confidence: `correction` 0.75 (tested), `direct\_statement` 0.70 (tested), `controlled\_test` 0.65 (tested), `inferred\_pattern` 0.30, `single\_observation` 0.15. Reinforcement is multiplicative toward the bound (`c' = c ± step·(1−c)` / `c' = c − step·c`) with steps 0.60/0.30/0.25/0.08/0.04, so weak evidence structurally cannot carry a belief fast — 0.30 at step 0.08 needs \~17 consistent observations to reach 0.75. Untested ceiling: `c.min(0.75)`.



Block layout and cadence:



| Section | Cadence | Source |

|---|---|---|

| `\[What I know about you]` | \*\*every turn\*\*, ≤6 beliefs | DPP over candidate set |

| `\[Worth testing this turn]` | \*\*every turn\*\* a scheduled assumption exists | wildcard slot |

| `\[Where I'm unsure]` | \*\*every 12 exchanges\*\*, ≤2 lines | uncertainty slot + novelty |

| `\[Check yourself]` | \*\*on detected plateau only\*\*, 14-day dismissal cooldown | curiosity nudge |



Footer every turn: \*"This is a model of you, not a fact about you. If any of it is wrong, say so and I'll drop it."\* — that footer is what tells the model it's allowed to push back on its own memory.



\*\*DPP over retrieval is the highest-leverage reuse in the port.\*\* Kernel `W·S·W` with `S` = cosine similarity between belief embeddings, `W = diag(effective\_weight)`. It's what stops "user is technical" from crowding out "user gets frustrated with jargon": the rank-1 downdate suppresses a near-duplicate only when the first is \*much\* stronger, so near-equal weights guarantee both appear.



Hard cap 350 tokens via the daemon's `count\_text\_tokens`. Truncation order: `\[Check yourself]` → `\[Worth testing]` → uncertainty lines → weakest beliefs. Never mid-line. Sort `(effective\_weight desc, belief\_id asc)` so unchanged state renders byte-identical.



\*\*Assumption state machine:\*\* flag at `confidence ≥ 0.55 AND !tested` → `scheduled` at +20 exchanges → `surfaced` on render → `passed`/`failed` on the next `direct\_statement`/`correction`/`controlled\_test` touching it → `stale` after 60 unresolved exchanges (plus the ×0.5 penalty). The 30-day re-evaluation rule reuses this machinery: subtract 0.15 and re-enter at `scheduled`.



\*\*Contradictions are preserved, never silently resolved.\*\* Two triggers: model-reported via the schema's `contradicts` field (always trusted), and engine-side cosine ∈ \*\*\[0.72, 0.93]\*\* with opposite mean polarity (above 0.93 → merge instead; below 0.72 → unrelated). On detection: `contradictions(status='open')`, `contradict\_count += 1` on both, and \*\*no confidence update\*\* — the ×0.5 weight plus `\[Where I'm unsure]` surfacing \*is\* the mechanism.



\## Consolidation



`adaptive\_pathway::background::run(engine, host\_pool, chat, cfg, shutdown\_rx)`, spawned in `bigtiny\_rust/src/lib.rs` alongside the scheduler, `.abort()`ed before `agent.shutdown()`. One 60s interval, two duties.



\*\*Idle sweep, every tick.\*\* `status='idle' AND updated\_at < now-15min`, \*\*plus\*\* `status='active' AND updated\_at < now-30min` — the second clause catches sessions whose daemon was killed mid-turn, which would otherwise stay `active` forever and never consolidate. Skip paused. Max `idle\_sweep\_batch` (3) per tick so a daemon restarting after a long absence doesn't fire 40 constrained-decode requests at once. Per session: `extract\_and\_record(IdleClose)` on the unlearned tail, then `consolidate\_session`.



\*\*`consolidate\_session`:\*\* load conversation-layer beliefs → discard `confidence < 0.25` or single-observation-with-support-1 (keep the observations as audit trail) → merge into context at cosine ≥ 0.86 (bump support/sessions, recompute confidence, take max provenance, re-parent observations) → \*\*promote context → identity only when all four gates hold\*\*: `support\_count ≥ 3` AND `distinct\_sessions ≥ 2` AND (`provenance ∈ {direct\_statement, controlled\_test, correction}` OR `tested`) AND `confidence ≥ 0.65`. The two-session gate is what stops one chatty conversation writing a permanent fact about someone. Then contradiction pass, write `consolidated\_at`, clear `open\_topics`, \*\*keep the row\*\* (it holds the pause flag and exchange counter).



Shutdown doesn't try to drain: `abort()` leaves `consolidated\_at` NULL and the next boot's first sweep picks it up.



\## MCP tools



\*\*`record(observation, provenance, domain\_hint?)`\*\* — `provenance ∈ {direct\_statement, controlled\_test, inferred\_pattern, correction}`. Inline embed + upsert under a 3s budget; failures return a soft message, never an error the model must reason about.



\*\*`forget(what, reason?)`\*\* — `reason ∈ {wrong (default), outdated, private}`. Takes no belief id: the engine resolves `what` against `conversation\_state.last\_recall\_ids` (the exact ids injected into this session's most recent block) with fuzzy text match, else top-1 cosine above 0.80. Asking a 7B model to echo a UUID is exactly what it gets wrong.



\- `wrong` → permanent suppression + a `contradictions` row at `resolved\_b` so the death is auditable + tombstone so extraction can't relearn it

\- `outdated` → 90-day suppression; may re-earn confidence

\- `private` → \*\*hard delete\*\* belief + observations + assumption + FTS row, plus a permanent `forget\_tombstones` row keyed on text hash



Returns the exact text dropped, phrased so the model echoes it ("Dropped: 'You prefer terse code comments.'"). \*\*That echo is the in-chat correction UX — no frontend work needed.\*\*



\*\*Incognito is not a tool\*\* — exposing it invites the model to disable its own memory. It's `POST /api/pathway/sessions/{id}/pause`, stored in `conversation\_state.paused`, mirrored to a `DashMap` for the hot path. Paused: `recall()` returns `None` (\*\*zero prompt delta, so the KV prefix is literally unchanged\*\* — a free property), `record`/`forget` return "memory is paused," both learn seams skip, consolidation never retroactively learns the paused stretch.



\*\*Deletions in `loop\_.rs`:\*\* `AUTO\_INVOKED\_AP\_TOOL\_NAMES` (127), `ADAPTIVE\_PATHWAY\_TOOL\_NAMES` (332), `AP\_CONTEXT\_MAX\_CHARS` (352), `reward\_from\_tool\_result` (224), `spawn\_record\_outcome` (1157-1180), `render\_decide\_hints` (200-217), `adaptive\_decide` (1086-1118), `llm\_visible\_tools\_openai\_format` (134). Then check whether `crate::pyrepr` has any remaining consumers.



\## Host and frontend



\*\*`/api/pathway/\*`\*\* in a new `plugins/bigtiny\_rust/src/routes/pathway.rs`, following `routes/memory.rs:24-31` (`/api/memory/stats`) exactly — same shape, same consumer, same cadence. It inherits `X-API-Key` auth, which the current sidecar \*\*has no equivalent of at all\*\*. `AppState` gains `pathway`; it has no `Default`, so that touches `lib.rs:129-137` and `tests/routes\_smoke.rs:60-68` (grep for a third site).



\*\*`src-tauri`:\*\* move `src/adaptive\_pathway/mod.rs` → `src/bigtiny/pathway.rs`, re-point at `/api/pathway/\*` through `bigtiny::client` (note: \*\*no `put\_json` helper exists\*\* — add one or switch the two PUT routes to PATCH). Commands go 20 → \~14, keeping names where they survive. `require\_ok` collapses to "is BigTiny up," i.e. let `ensure\_client` fail.



\*\*Retire:\*\* `lifecycle/adaptive\_pathway\_proc.rs` (305 lines: pidfile, `kill\_stale\_orphan`, readiness poll), `lifecycle/mod.rs:205-272` step 2c, `AppState.adaptive\_pathway\*` (`state.rs:100-109`), `adaptive\_pathway\_port`/`\_launch\_command`/`\_launch\_args` + `migrate\_ap\_launch\_command`.



\*\*Frontend:\*\* dies — `HintBadge`, `HintFeedbackButtons`, `NudgeConsentPrompt`, `SchismResolutionModal`, `tryParsePyRepr` and its callers in `stores/chat/messageUtils.ts:148,171`, the schism event plumbing. Rebuilt in place — `AdaptivePathway.tsx` (drops launch/port fields), `GraphHealth.tsx` → belief-health, `DomainProfiles.tsx` → belief domains. Genuinely new — \*\*belief browser\*\* (list, confidence, provenance, tested/untested, contradiction pairs with Keep A/B/Both/Neither, per-belief delete), \*\*export to file\*\*, \*\*incognito toggle\*\* (repurposing `AdaptivePathwayToggle.tsx`).



\## Phasing



Every phase compiles and is independently verifiable. The Python plugin stays untouched on disk until Phase 8.



\- \*\*Phase 0 — Scaffold + pure primitives.\*\* Crate, `error.rs`, `config.rs`, `embed/`, `vector/`. \*\*First action, before any code: add `sqlx = "0.8"`, add the path dep, run `cargo tree -d -p libsqlite3-sys` and confirm exactly one.\*\* That's Risk 1 and it's five minutes now versus days later. \*Accept:\* `cargo test -p adaptive\_pathway` green with ported `test\_embeddings`/`test\_diversity`/`test\_novelty` assertions.

\- \*\*Phase 1 — Store + schema.\*\* Migrations, `store/`, `Db::open`/`open\_in\_memory`. \*Accept:\* in-memory round-trip of a belief with a BLOB embedding through insert → select → `\&\[f32]`; every declared index asserted present via `sqlite\_master`.

\- \*\*Phase 2 — Engine + recall (standalone).\*\* `layers.rs`, `belief/`, `domains.rs`, `recall/`, `antisycophancy.rs`, `engine.rs`. \*Accept:\* seed \~30 beliefs across 3 layers / 2 domains, then assert block under cap; cross-domain downweighted not excluded; untested 0.80 ranks below tested 0.55; DPP surfaces two similar-but-opposed rather than two near-identical; byte-identical render on repeat; paused → `None`.

\- \*\*Phase 3 — Learn + consolidate (standalone).\*\* `learn/`, `consolidate.rs`, `maintenance.rs`, `background.rs`, `traits.rs`. \*Accept:\* extraction against a mock produces expected rows; concurrent second pass → zero new rows; watermark never decreases; promotion blocked by each of the four gates individually.

\- \*\*Phase 4 — MCP server + devtool.\*\* `record`/`forget`, `serve\_in\_process`, `bin/devtool.rs`. \*Accept:\* in-crate test connects over `tokio::io::duplex`, round-trips `tools/list` + a `record` call.

\- \*\*Phase 5 — Daemon read path + routes.\*\* Path dep; engine construction + `MCPManager::with\_pathway` + `AppState.pathway` + background spawn/abort; `builtin.rs` widened + `"pathway"` arm + `BUILTIN\_SERVERS: \[\&str; 4]`; `manager.rs:59-64`; `AgentLoop.pathway` and `adaptive\_decide` → `pathway\_recall`; `builder.rs` header + param rename; `routes/pathway.rs`; the `StructuredChat` impl. \*\*Read path before write path deliberately\*\* — with `GET /api/pathway/beliefs` live and the devtool seeding, you can eyeball real recall output before committing to the extraction prompt. \*Accept:\* `cargo test -p bigtiny\_rust` fully green, especially the byte-identity test.

\- \*\*Phase 6 — Write path + delete old hooks.\*\* Turn-end call at `loop\_.rs:648`; `CompactionResult` fields + spawns at `loop\_.rs:1058` and `routes/chat.rs:436`; all the `loop\_.rs` deletions. \*Accept:\* a real multi-turn Ollama session accumulates plausible beliefs; watermark never regresses.

\- \*\*Phase 7 — Kitty host.\*\* \*\*First, as its own earlier commit: re-home `self\_heal\_builtin\_servers` and `ensure\_embedding\_model` off the AP health loop\*\* (Risk 12). Then register under the new name `"pathway"` with `transport: "in\_process"`; add `"adaptive-pathway"` to `RETIRED\_BUILTINS`; add `transport` to `decide\_sync\_action`'s diff \*and\* patch; add `"pathway"` to `HIDDEN\_SERVER\_NAMES`; retire the supervision path; move and rewrite the client. \*Accept:\* app launches; daemon log shows `pathway` connected \*\*via in-process transport — first Windows consumer, verify explicitly\*\*; Settings shows `tool\_count: 2`.

\- \*\*Phase 8 — Frontend, packaging, retire Python.\*\* All TS work. Then remove the two `PLUGINS` entries in `plugins/build.py`; remove both `externalBin` entries from `tauri.conf.json`, \*\*then\*\* delete the placeholder `.exe`s — order matters, because Tauri validates every \*listed\* entry exists on disk even for a plain `cargo check`. `git rm -r plugins/adaptive-pathway`, moving the two `.md` docs into the new crate. Update `docs/PLUGINS.md`, `docs/ADAPTIVE\_PATHWAY.md`, `CLAUDE.md`.



\## Verification



\*\*Port directly\*\* (math unchanged): `test\_diversity.py` → `vector/dpp.rs` (kernel, downdate, all-nonpositive-diagonal fallback, `k>n`, `n==0`); `test\_novelty.py` → `vector/cms.rs`; `test\_embeddings.py` → `embed/` (mockito for Ollama, fallback, projection at 128/384/768/1024, hashing determinism, LRU, probe backoff); `test\_ttl.py` → `store/suppressions.rs`.



\*\*Port the intent:\*\* `test\_hardening.py` (garbage inputs never panic across `recall`/`record`/`forget`), `test\_multi\_session.py`, `test\_history\_persistence.py`, `test\_health.py`, the entropy half of `test\_curiosity.py`. \*\*`test\_kitty\_contract.py` is the highest-value port\*\* — rewrite as `plugins/bigtiny\_rust/tests/pathway\_contract.rs` asserting every `/api/pathway/\*` response carries the keys the Rust client and TS types need. That's the test that catches a route rename silently breaking the Settings pane.



\*\*Do not port:\*\* `test\_ensemble`, `test\_thompson`, `test\_paradigm\_challenge`, `test\_in\_session`, `test\_blending`, `test\_sidecar`, `test\_mcp\_server`, and the selector portions of `test\_engine`/`test\_discovery`.



\*\*New, no Python precedent:\*\* provenance → confidence table; the ×0.625 rule reproducing 0.80 → 0.50 exactly; the assumption state machine; the contradiction band (0.70 no / 0.85 yes / 0.95 merge); each promotion gate blocking individually; \*\*learn-watermark no-double-count\*\* (run twice → zero new; out-of-order lower `through\_rowid` → no regression); recall cap + truncation \*order\*; render byte-stability.



\*\*KV-cache guard — the important one.\*\* Keep the existing byte-identity test green and add a companion asserting (a) with `Some(block)`, only indices ≥ `head.len()` differ from baseline, and (b) with `None`, output is byte-identical to the no-pathway baseline.



\*\*End-to-end:\*\* test `build\_messages` directly with a hand-built block (that's where the risk is, and it avoids constructing a full `AgentLoop`); integration-test the write hook against in-memory `bigtiny.db` + `pathway.db` with a mock `StructuredChat`; adding `"pathway"` to `BUILTIN\_SERVERS` auto-covers the in-process arm \*provided\* the test passes a real `PathwayEngine::open\_in\_memory()` — which is the concrete payoff of signature-widening over a global. Plus a manual 10-turn smoke script: state a preference, contradict it three turns later, say "actually forget that," check the browser after each step.



\## Risks



1\. \*\*`libsqlite3-sys` unification — can kill the approach on day one.\*\* Cargo permits exactly one crate with a given `links` key per binary. Verified: `bigtiny\_rust/Cargo.toml:29` is `sqlx = "0.8"` and `libsqlite3-sys` is in the lock. Pin `sqlx = "0.8"` and check `cargo tree -d` first thing in Phase 0. (This is why kitty-tools' different rmcp major is harmless — rmcp declares no `links`.)

2\. \*\*First Windows in-process consumer.\*\* `connect\_in\_process` and `builtin.rs` have only run in unit tests and on Android; nothing on Windows has reached `manager.rs:59-64`'s `InProcess` branch \*from a DB row\*. Verify `TransportType` deserializes `"in\_process"`, that `row\_to\_config` tolerates NULL `url` + non-path `command`, and that a failed connect surfaces as `status='error'` rather than hanging to the 60s timeout. Land the transport flip as its own commit.

3\. \*\*Transport-migration gotcha — verified real.\*\* `decide\_sync\_action` (`bigtiny/mcp.rs:461-465`) diffs `command`/`args`/`env`/`enabled` but \*\*not\*\* `transport`, and the patch it builds sets none. An existing `adaptive-pathway` row would be PATCHed to the logical command while keeping `transport: "stdio"`, then try to `exec("pathway")` and sit in `error` forever with no obvious cause. Do \*\*both\*\* mitigations: register under the new name + `RETIRED\_BUILTINS` (delete-then-create \*is\* the migration, and it drops the obsolete `AP\_SIDECAR\_PORT` env), \*\*and\*\* add `transport` to the diff as the latent-bug fix.

4\. \*\*KV-cache byte-identity\*\*, three ways to break it: moving injection into the stable head (tempting — the block reads like persona); the block churning on a recency tie (mitigated by day-quantization + id tie-break); returning `Some("")` instead of `None`. Update the doc comment at `builder.rs:43-55` to name the new block.

5\. \*\*Summarizer contention on Ollama.\*\* llama.cpp serializes per slot, so two concurrent constrained decodes roughly double p99 — which can push a compaction pass past its stale-lock reclaim window. Four mitigations, all of them: per-session learn lock; compaction piggyback sequenced \*after\* the memory-slot call; a \*\*global 1-permit semaphore\*\* around every `structured\_chat`; `learn\_every\_n=4` and `idle\_sweep\_batch=3`. Above all, every learn path is spawned and best-effort — worst case is "learning is slow," never "the turn is slow."

6\. \*\*Extraction quality at 1.2B — the biggest \*product\* risk.\*\* `LFM2.5-1.2b` asked for behavioral observations will emit task summaries and over-claim `direct\_statement`. Mitigations: prompt framing, the hard 5-observation cap enforced in Rust, the schema-level exclusion of `identity`, the untested ceiling, and the fact that a bad belief costs one line the user can delete. \*\*The most important mitigation is sequencing\*\* — Phase 5 before Phase 6, so you see real output before committing to the prompt.

7\. \*\*Deleting the AP health loop silently kills two unrelated subsystems — verified.\*\* `spawn\_adaptive\_pathway\_health\_loop` (`lifecycle/health.rs:41`) hosts three tenants: AP status/schism polling (dies), `self\_heal\_builtin\_servers` at line 150 gated behind `if up \&\& tick % 24 == 0` where `up` means \*the AP sidecar is up\* — it reconnects \*\*every\*\* bundled MCP server (kitty-tools, kitty-web, kitty-wasm), and `ensure\_embedding\_model` at line 196. Deleting wholesale degrades the whole app: dropped MCP servers stay dead until restart, and a model pulled after launch is never detected. Re-home both survivors as a \*\*separate, earlier commit\*\*, verified independently.

8\. \*\*Two WAL files under a OneDrive-synced path.\*\* The daemon already has this exposure for `bigtiny.db`; a second file doubles the surface without changing it qualitatively. Put `pathway.db` in the same directory so any future "move the data root off OneDrive" fix covers both.

9\. \*\*The `require\_ok` string contract.\*\* `GraphHealth.tsx:20` string-matches the exact error text. Keep the literal through Phase 7; delete the match in the same commit as the component rewrite in Phase 8.



\## Critical files



\- `plugins/bigtiny\_rust/src/agent/loop\_.rs` — read hook (562/1086), turn-end seam (648), compaction seam (1058), largest block of deletions

\- `plugins/bigtiny\_rust/src/agent/context/builder.rs` — layer-7 tail injection and the KV-cache contract

\- `plugins/bigtiny\_rust/src/mcp/builtin.rs` + `mcp/manager.rs:59-64` — shared-engine resolution, new in-process arm

\- `plugins/bigtiny\_rust/src/lib.rs` — engine construction, `AppState`, background spawn/teardown

\- `plugins/bigtiny\_rust/src/agent/compaction.rs` — `CompactionResult`, the reusable masked chunk at 924

\- `src-tauri/src/bigtiny/mcp.rs` — `ensure\_builtin\_servers` (259-273), `RETIRED\_BUILTINS` (195-201), `decide\_sync\_action` (457-492)

\- `src-tauri/src/lifecycle/health.rs` — the three-tenant loop that must be carefully unpicked

