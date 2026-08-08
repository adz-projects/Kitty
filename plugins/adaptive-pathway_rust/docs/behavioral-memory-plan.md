# Behavioral Memory System — Transition Plan

## 1. Core Shift

**From:** Action selection — "which tool/approach should I use?"
**To:** Behavioral modeling — "who is this user, what do they care about, and how should I adapt?"

The graph structure stays. Edges become learned facts about the user rather than tool preferences. Confidence becomes "how well-established is this belief?" Novelty becomes "what about them have we not yet discovered?" The system learns *about the user* instead of learning *which actions to take*.

This is a rewrite, not an extension. The current plugin's decision loop (ensemble scoring → DPP selection → hints) solves a different problem than what we need. We keep the storage layer, embedding infrastructure, and several anti-sycophancy mechanisms. Everything else changes.

---

## 2. What Changes and Why

### The Three-Layer Memory Model

| Layer | Speed | Content | Lifetime |
|---|---|---|---|
| **Identity** | Slow | Communication style, values, recurring interests, core preferences | Persistent across all sessions, decays very slowly |
| **Context** | Medium | Current projects, active goals, recent topics, work patterns | Lives for weeks/months, fades when dormant |
| **Conversation** | Fast | Immediate context, tone, exchange patterns, open threads | Lives for the session, consolidates into slower layers at close |

The current system has one flat graph. The new system has three graphs with different decay rates, retrieval thresholds, and consolidation rules. A belief about how the user approaches code reviews matters when they're coding but shouldn't surface when they're asking about cooking — domain bleed becomes the primary routing mechanism, not a dampener.

### Belief Formation and Provenance

Every belief carries metadata about *how it was learned*:

| Provenance | Source | Initial Confidence |
|---|---|---|
| `direct_statement` | User explicitly stated a preference | High (0.70) |
| `controlled_test` | System tried alternatives, user responded differently | High (0.65) |
| `inferred_pattern` | Repeated observation without active testing | Low (0.30) |
| `single_observation` | One ambiguous exchange | Very low (0.15) |

The current system infers confidence from reward magnitude and frequency. The new system infers it from *how the evidence was gathered*. A belief from three consistent interactions is different from one from a single ambiguous exchange, even if both have high frequency scores.

### Untested Assumptions as First-Class Objects

An untested assumption is a belief that reached medium-or-high confidence purely through repeated pattern matching, without active testing. This is the sycophancy vector: the system observes consistency and mistakes it for preference, when it might just be habituation.

**Lifecycle:**

```
Observation → Assumption (untested) → Scheduled active test → Tested belief OR corrected
                                                    ↓
                                          If never tested → flagged as stale
                                                    ↓
                                          Eventually deprioritized or discarded
```

**How the system identifies them:** Any belief that crossed a confidence threshold without passing through `controlled_test` provenance is flagged. The system tracks *how* each belief was formed and separates inference from validation.

**How the system uses them:**
- Lower effective weight: an untested assumption at 0.80 confidence behaves like a tested belief at 0.50
- Scheduled testing: when an untested assumption crosses an age threshold (configurable, ~20 exchanges), the sidecar flags it and the model is prompted to try the alternative
- Visible in recall: tagged as "untested — inferred from pattern" so the AI knows to hedge or ask
- Escalation under contradiction: when new evidence conflicts, the system surfaces the tension rather than quietly updating confidence

### Anti-Sycophancy Strategy

The current plugin has four mechanisms (novelty bonus, uncertainty slot, wildcard slot, curiosity nudge) that force exploration. In a behavioral memory system, these become *explicit signals* rather than hidden scoring bonuses:

| Current Mechanism | New Role |
|---|---|
| **Novelty bonus** → Unknown-unknown surfacing | When the system has learned "user prefers X," it occasionally surfaces "but they also engaged deeply with Y — is X a preference or just habit?" Holds multiple conflicting models without collapsing too quickly |
| **Uncertainty slot** → Explicit doubt | Maintains a running list of uncertain beliefs. Surfaces them as metadata in recall: "I think you prefer direct answers, but I'm not confident." Lets the user correct the model |
| **Wildcard slot** → Constructive disagreement | The system occasionally recommends going against its own learned preferences. If the model says "user prefers short answers," the wildcard mechanism surfaces "try a longer, more exploratory response — they might benefit from it even if they don't ask for it" |
| **Curiosity nudge** → Preference stress-testing | Detects when the AI has been consistent (always giving short answers, always being direct) and flags the pattern: "Am I serving you well, or am I just doing what you've gotten used to?" |

**Additional mechanisms:**

- **Preference expiration with re-evaluation:** Learned preferences periodically require active confirmation. After 30 days of "user prefers X," the system runs a controlled test: offer X half the time, offer something different half the time, see if the user notices. No reaction → the preference was inertia, not desire.
- **Explicit contradiction tracking:** When new evidence conflicts with an established belief, the system preserves the contradiction and makes it visible rather than silently updating confidence. Forces resolution.
- **Diversity of memory recall, not just suggestions:** DPP operates on *what gets retrieved from memory*, not just what gets output. If the system always recalls "user is technical" and never recalls "user gets frustrated with jargon," that's a retrieval bias. Diversity prevents this.
- **A "disagree with the model" channel:** The AI can say "the memory system suggests doing X, but I think Y would be better here." Makes the tension visible rather than buried in scoring.

The key shift: anti-sycophancy moves from *mechanism* to *conversation*. The system doesn't protect against sycophancy by forcing exploration — it protects against it by inviting the user to correct it.

---

## 3. New Architecture

### Tool Surface: One Tool

| Tool | Purpose | Called By |
|---|---|---|
| `record(observation, provenance)` | Log a nuanced observation about the user | Model, when it notices something worth remembering |

That's it. The model calls one tool when it has something meaningful to contribute. Everything else happens automatically.

**Why one tool:** Smaller models struggle with 15 tools. They skip calls, call the wrong one, or ignore the system entirely. One tool is reliable. The backend handles the rest.

**What `record` takes:**
- `observation`: natural language description of what was noticed ("User seemed frustrated when I used jargon")
- `provenance`: how it was learned (`direct_statement`, `inferred_pattern`, `correction`, `controlled_test`)
- Optional: `domain_hint` if the observation is topic-specific

The sidecar handles everything downstream: updating beliefs, creating/modifying edges, tracking novelty, testing assumptions, managing TTLs.

### Backend Hooks: Two Intercepts

**Read hook (automatic recall):** At the start of each AI turn:
1. Backend takes conversation context, sends to sidecar
2. Sidecar returns relevant memories with flags (tested/untested/contradicted/uncertain)
3. Backend injects a compact block into the system prompt

The model never calls a recall tool. It just receives memories as part of its context. Deterministic — happens every turn, no exceptions.

**Write hook (automatic signal extraction):** After each exchange:
1. Backend analyzes observable signals (response length, engagement depth, topic shifts, corrections, emotional valence)
2. Sends low-confidence, high-volume inputs to sidecar
3. Sidecar updates beliefs continuously from these signals

The model doesn't need to call anything for basic learning to happen. The backend captures patterns automatically. If the model also calls `record` with nuanced observations, those carry higher weight and more specific provenance.

**Trade-off:** The backend can't distinguish nuance the way a model can. A short response might mean "I'm busy" not "I prefer brevity." The automatic signals are low-confidence inputs that feed the system continuously but rarely drive beliefs alone. The model's `record` calls add judgment when warranted.

### Sidecar Autonomy: Background Work

These happen on timers or thresholds, never called by the model:

| Task | Trigger | What it does |
|---|---|---|
| Session boundary detection | Activity timeout | Consolidates conversation layer into context/identity layers |
| Assumption testing | Untested assumption crosses age threshold (~20 exchanges) | Flags for model to surface via `record` or system prompt |
| Plateau detection | Conversation entropy drops below threshold | Injects nudge into next recall response |
| Preference re-evaluation | Tested belief crosses staleness threshold (~30 days) | Schedules controlled test, widens confidence bounds |
| Maintenance | Nightly timer | Confidence decay, history pruning, tier management, domain sync |
| Health check | Nightly timer | Logs system state, flags anomalies |
| Contradiction resolution | Conflicting evidence detected | Surfaces tension in next recall, doesn't silently update |

The sidecar maintains itself regardless of what the model does. If the model forgets to call `record`, the system degrades gracefully — it just learns slower from subtle signals.

---

## 4. Migration Path

### Phase 1: What to Keep from Adaptive Pathway

| Component | Reuse? | Notes |
|---|---|---|
| `embeddings.py` (EmbeddingProvider) | Yes | Ollama fallback to hashing works as-is |
| `storage/database.py` (SQLite schema) | Partially | Keep table structure, repurpose columns for beliefs instead of edges |
| `storage/tiered.py` (TieredCache) | Yes | Hot/warm/cold tiers map to conversation/context/identity layers |
| `storage/vec.py` (VectorIndex) | Yes | Cosine similarity retrieval is still the core lookup |
| `decision/novelty.py` (CountBasedNovelty) | Yes | Count-Min Sketch for novelty tracking still applies |
| `decision/diversity.py` (DPP selection) | Yes | Repurpose for memory retrieval diversity, not hint diversity |
| `integrations/sidecar/server.py` (FastAPI skeleton) | Yes | HTTP server structure stays, routes change |
| `config/defaults.yaml` | Partially | Keep structure, rewrite parameters for belief management |

### Phase 2: What to Rewrite

| Component | Why |
|---|---|
| `engine.py` | The decision loop (ensemble → DPP → hints) solves a different problem. New engine manages three memory layers, belief lifecycle, and hook integration |
| `decision/ensemble.py` (BootstrapEnsemble) | Thompson sampling over action buckets doesn't apply to behavioral beliefs. Replace with belief confidence management |
| `decision/thompson.py` (ThompsonLinUCB) | Multi-armed bandits are for action selection, not belief tracking |
| `decision/info_gain.py` | Variance reduction scoring doesn't map to memory relevance |
| `decision/paradigm_challenge.py` | PC signals are about domain isolation in tool selection. Replace with assumption testing and contradiction tracking |
| `decision/blending.py` | Edge blending becomes belief synthesis — needs rewrite |
| `decision/selector.py` (ActionSelector) | No action selection in the new system |
| `decision/in_session.py` (InSessionBandit) | Per-session bandit doesn't apply. Replace with conversation layer tracking |
| `learning/bleed.py` (DomainBleed) | Domain dampening becomes context routing — rewrite for relevance-based filtering |
| `learning/ttl.py` (EdgeTTL) | TTL suppression still applies (suppressing wrong beliefs), but needs integration with contradiction tracking |
| `learning/preferences.py` (PreferenceDetector) | Centroid-based preference detection is close to what we want, but needs rewrite for belief provenance and untested assumption tracking |
| `discovery/domains.py` | Domain discovery still useful (topic areas where behavioral patterns cluster), but needs integration with three-layer model |
| `discovery/primitives.py` | Semantic primitive tracking becomes belief topic tracking — rewrite |
| `mcp_server.py` | 15 tools → 1 tool. Complete rewrite of tool surface |

### Phase 3: What to Add

| New Component | Purpose |
|---|---|
| `belief_lifecycle.py` | Manages the lifecycle of beliefs: observation → assumption → test → tested/corrected. Handles untested assumption tracking and scheduled testing |
| `contradiction_tracker.py` | Detects and preserves contradictions between beliefs. Surfaces tension instead of silently updating confidence |
| `hook_handler.py` | Processes backend hook requests: automatic recall (read) and automatic signal extraction (write). Formats system prompt injection |
| `provenance_engine.py` | Assigns initial confidence based on how evidence was gathered. Tracks provenance metadata per belief |
| `conversation_consolidation.py` | At session close, consolidates conversation-layer observations into context/identity layers. Decides what to promote and what to discard |
| `anti_sycophancy.py` | Coordinates the four repurposed mechanisms (unknown-unknown surfacing, explicit doubt, constructive disagreement, preference stress-testing). Outputs signals that surface through recall |

### Phase 4: Database Schema Changes

Current schema tracks edges (semantic primitives with confidence, domain, tier, frequency). New schema tracks beliefs:

| Current Table | Becomes |
|---|---|
| `edges` → `beliefs` | Core table: text, embedding, confidence, provenance, layer (identity/context/conversation), tested flag, domain, tier, co-selection |
| `nodes` → stays | Context embeddings linked to beliefs |
| `ensemble_state` → deleted | No Thompson sampling over actions |
| `annotations` → `observations` | Records of model-reported observations with provenance |
| `ttl_entries` → stays | Active suppressions for wrong beliefs |
| `domains` → stays | Topic areas where behavioral patterns cluster |
| `feedback_centroids` → deleted | Replaced by provenance-based confidence |
| `action_history` → deleted | No action selection |
| `novelty_history` → stays | Novelty tracking still applies |
| `blended_edge_log` → `synthesis_log` | Tracks combined beliefs |
| `co_selection_log` → stays | Tracks which beliefs appear together |
| `app_settings` → stays | KV store for tunable weights |

New tables:
- `assumptions` — untested assumptions with age, confidence, and scheduled test status
- `contradictions` — conflicting belief pairs with resolution status
- `conversation_state` — per-session conversation layer data (open topics, tone, exchange patterns)

---

## 5. Open Questions

1. **How much memory gets injected per turn?** The recall block in the system prompt needs a hard cap. What's the right size? 5 memories? 10? This is a context window vs. recall quality trade-off.

2. **How does the backend extract signals without a model?** The automatic write hook needs to analyze engagement depth, emotional valence, and topic shifts. This could be rule-based (response length thresholds, keyword matching) or it could use a small classifier. Rule-based is cheaper but less nuanced.

3. **How aggressive should assumption testing be?** If the system tests assumptions too often, it feels erratic. Too rarely, and untested beliefs harden into false certainty. The threshold (~20 exchanges) needs tuning based on user feedback.

4. **Should the model see that it's using a memory system?** Transparency vs. friction. If the system prompt says "here are memories about this user," the model knows to use them. But should the *user* know? Surfacing "I remember you prefer X" can feel helpful or creepy depending on context.

5. **Multi-user support?** The current system uses `session_id`. A behavioral memory system needs `user_id` for persistent identity across sessions, plus session-scoped conversation state. How are users identified?

6. **Memory editing and deletion?** Users should be able to say "forget that" or "that's wrong." The system needs a direct correction path, not just gradual confidence decay.

7. **Privacy and data retention?** Behavioral memory is more sensitive than tool-use history. What gets stored, for how long, and can the user export or delete it? This shapes the storage layer design.

8. **How does domain routing work without explicit domains?** The current system uses centroid clustering of context embeddings. For behavioral memory, domains become topic areas (work, personal, creative). Should these be auto-discovered or user-defined?

---

## 6. Success Criteria

The new system succeeds if:

- Smaller models (7B class) use it reliably without tool-calling errors
- The system learns meaningful patterns about the user within a reasonable number of exchanges
- Anti-sycophancy mechanisms prevent the system from collapsing into habituation
- Untested assumptions are identified, flagged, and eventually tested or discarded
- The model can surface doubt and invite user correction naturally
- The system degrades gracefully when the model doesn't call `record`
- Recall is fast enough to be injected into every turn without noticeable latency
- The user feels understood, not mirrored
