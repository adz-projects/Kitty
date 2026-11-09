# Memory evals and baselines

Phase 0 of the memory revision plan: measure before changing. Three
measurements, each repeatable after every later phase.

## 1. Recall evals (both engines)

Labelled cases, as data:

- `pathway_cases.json`: one fictional user's beliefs. Each case is a message
  with the beliefs that should inject (`expect`), must not inject (`forbid`),
  and the recall sections that should appear (`expect_sections`).
- `memorabilia_cases.json`: documents with scripted claims, so the eval
  measures recall rather than the extraction model. Each case has a `kind`:
  `direct`, `indirect`, `superseded` or `negative`.

Both run against the real EmbeddingGemma model; relevance thresholds mean
nothing in the hash space the crates' unit tests use. From `BigTinyV2/daemon`:

```bash
KITTY_EMBED_MODEL="$LOCALAPPDATA/Kitty/models/embeddinggemma-2-text-270m.litertlm" \
KITTY_LITERT_LM_EMBED_LIB="<repo>/src-tauri/resources/litert-lm-embed.dll" \
  cargo test --release --features litert-embed --lib -- --ignored memory_eval --nocapture --test-threads=1
```

For the old EmbeddingGemma 1 `.tflite`, pass `KITTY_LITERT_LIB` (`libLiteRt.dll`)
and `KITTY_EMBED_TOKENIZER` (`tokenizer.json`) instead of
`KITTY_LITERT_LM_EMBED_LIB`.

`--release` matters: the tokenizer is very slow in a debug build. Each run
prints a per-case table and summary rates. It fails only on setup errors,
unless `MEMORY_EVAL_STRICT=1` is set, in which case any miss fails it.

With EmbeddingGemma 2 after Phases 2-4 (November 2026):

- Pathway: 27/30 expected beliefs injected, 2/112 forbidden leaked, every
  expected section present.
- Memorabilia: direct 8/8, indirect 1/1 (all 3 analogy targets reached),
  superseded 1/1, nothing leaked.

`[Worth challenging]` is expected only where a `stance` belief is expected,
because it never challenges context or preferences.

### Relevance floor sweep

`memory_eval_relevance_floor` (same command, filter `memory_eval_relevance_floor`)
scores every (question, memory item) pair in both fixtures by raw and by
mean-centred cosine. For each candidate floor it reports:

- the share of expected items that pass;
- the share of forbidden items that leak through;
- how many off-topic questions inject nothing.

It also prints the floor that maximises `expected − 2 × leaked`, weighting
leaks double because the plan is to err towards too selective. Run it after
changing the embedding model, since floors are properties of one embedding
space.

On EmbeddingGemma 2 (2026-11-09):

| | old floor 0.0 | chosen floor | at the chosen floor |
|---|---|---|---|
| Pathway (`recall.min_cosine`) | 97% expected, 38% leaked, 0/3 off-topic clean | **0.10** | about 93% expected, 3% leaked; 0.117 gives 90% expected, 0% leaked, 3/3 clean |
| Memorabilia (`recall_min_cosine`) | 100% expected, 26% leaked, 0/2 clean | **0.12** | 100% expected, 2% leaked, 2/2 clean, anywhere on the 0.106–0.36 plateau |

Mean-centred cosine separates as well as raw cosine or better on both, so the
floors stay centred. Pathway's identity floor (`min_cosine_identity`, 0.05) is
not measured: the fixture has no identity cases yet.

## 2. Overnight cluster sizes (Memorabilia)

Every overnight pass now reports `clusters_seen` and `largest_cluster`, and
logs a size histogram:

```
overnight_pass: recurrence clusters (size -> count) candidates=… clusters=… largest=… histogram={…}
```

It appears in `%APPDATA%\BigTinyV2\daemon.log` after a pass (*Process now* in
Settings → Memorabilia → Overnight learning runs one). A `largest` that is a
large share of `candidates` means the raw-cosine 0.80 threshold is chaining
unrelated topics together. That would decide the clustering fix, plan step 1.6.

## 3. Prompt-cache hit rate

Each session's last 100 model calls are kept in `sessions.metadata.usage`.
To read the per-chat hit rate (copy the database first; the engine holds it
open):

```sql
SELECT s.id,
       COUNT(*)                                                         AS calls,
       SUM(json_extract(u.value, '$.prompt_tokens'))                    AS prompt,
       SUM(json_extract(u.value, '$.cache_read_tokens'))                AS cached,
       ROUND(1.0 * SUM(json_extract(u.value, '$.cache_read_tokens'))
                 / SUM(json_extract(u.value, '$.prompt_tokens')), 3)    AS hit_rate
FROM sessions s, json_each(s.metadata, '$.usage') u
WHERE json_extract(u.value, '$.cache_read_tokens') IS NOT NULL
GROUP BY s.id
HAVING calls >= 6
ORDER BY s.updated_at DESC
LIMIT 20;
```

Rows without `cache_read_tokens` are excluded because the provider didn't
report them, which is not the same as a miss. Record the numbers for a few
multi-turn, tool-using chats before Phase 2. Phase 2 (saving the memory block
into history) should raise them.
