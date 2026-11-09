# Prompt determinism

Providers cache a request's **prefix**: Anthropic and OpenAI price cached input
tokens far below fresh ones, and a llama-server slot reuses its KV cache for
every leading token that matches the previous request. Both only pay off if the
bytes at the front of the prompt stay identical from one call to the next. This
note records what the engine does to keep them identical, and the rules any
change to the prompt has to respect.

## What sits in the prefix, in order

1. **Tools.** Anthropic orders a request tools → system → messages; OpenAI-
   compatible servers and llama.cpp chat templates render the tools into the
   system block. Either way, *any* change to the tool list invalidates the
   whole conversation behind it.
2. **The stable head** (`agent/context/builder.rs`): persona, session
   override, writable-dirs hint, workspace snapshot, `[Original request]`
   anchor, then the compaction memory block (`[CONSOLIDATED PROJECT MEMORY]`).
3. **The live tail**: the messages after `compacted_through_rowid`.
4. **Memory blocks, stored in history**: the memory recall block (Adaptive
   Pathway + Memorabilia) is injected just before the user message it belongs
   to, and **stored on that message** (`messages.memory_block`). Every later
   turn re-emits it in the same place, so the previous turn's whole prompt is
   a byte-exact prefix of the next one. A block is injected only when it
   differs from the last stored one (the delta rule), so an unchanged memory
   costs nothing. Branches copy the stored blocks with their messages.
   (Before this, the block sat unstored before the newest message, and the
   next turn re-processed the whole previous exchange.) Pre-flight recall and
   the `<think>` thought seed are still tail-only and unstored.

## What keeps it stable

- **Recall is a function of stored state.** Given the same stored state and
  the same message (plus the chat's previous user message, which drives the
  trajectory extrapolation), both memory engines render the same block: no
  in-memory history, and nothing recall writes changes a later recall of
  the same turn. The self-tuning relevance floor reads a snapshot that moves
  only in the engines' nightly/heavy maintenance (each turn merely logs its
  cut, `recall::cut_log`); `[Check yourself]`'s streak is per chat and moves
  at turn end (`PathwayEngine::record_turn_end`); a judge verdict is cached
  per chat and message. A recall that misses its time budget still yields no
  block, which is the one remaining source of variation.

- **Tools are sorted by name** (`mcp/manager.rs`, `list_tools` /
  `list_tools_for_app`). Registration order would otherwise depend on which
  MCP server connected first. `CacheConfig::sort_tools` documents the intent;
  sorting is unconditional.
- **Slot pinning** (`id_slot`): a session always lands on the same
  llama-server slot, `fnv1a64(session_id) % parallel_slots`, for providers
  that set `ProviderConfig::parallel_slots`. FNV rather than `DefaultHasher`
  because the latter is not stable across Rust releases.
- **Anthropic `cache_control`** (`CacheConfig::anthropic_cache_control`): one
  ephemeral breakpoint on the joined system block caches tools + system.
- **Per-chat tool groups** (`agent/tool_gate.rs`, when an app configures a
  decision model with tool gating on): a chat's tool set is decided once and
  only ever **grows**; it is re-decided, and may shrink, only at a moment when
  the cache is already cold:
  - compaction has advanced `compacted_through_rowid` (the head was rewritten
    anyway);
  - the chat has been idle longer than the provider's `cache_ttl_secs`;
  - the chat is a new branch or moved to another card.

  Adding a group costs exactly one miss. If the decision model is unreachable,
  a chat with no recorded set gets every tool and records nothing, so the first
  real decision never shrinks a warm prefix.

## What still breaks it (known)

- **Compaction** rewrites the memory block and moves the live tail's start —
  unavoidable, and the reason compaction counts as a cold moment.
- **Masking** of old tool results is keyed to a reserve floor that moves as
  the tail grows, so older messages are occasionally re-masked.
- **Wrap-up** sends the last call with no tools, and the **step nudge**
  appends `request_more_steps` to the list mid-turn. Both change the tools
  block. Follow-up: keep the tool list fixed and steer with `tool_choice`
  instead.

## Measuring it

Every model call's usage is appended to the session's `metadata.usage[]`
(last 100) with `prompt_tokens`, `completion_tokens` and, when the provider
reported them:

| field | Anthropic | OpenAI-compatible | llama-server |
|---|---|---|---|
| `cache_read_tokens` | `cache_read_input_tokens` | `prompt_tokens_details.cached_tokens` | same, else `timings.cache_n` |
| `cache_creation_tokens` | `cache_creation_input_tokens` | — | — |

`prompt_tokens` includes the cached tokens on every provider (Anthropic's
`input_tokens` is normalized to include them), so the hit rate of a call is
`cache_read_tokens / prompt_tokens`. A missing key means "not reported", not
zero. A healthy chat shows a high ratio on every call after the first, with
single dips where a tool group was added or a cold moment re-decided the set.
