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

Some cases are expected to fail until a later phase lands. Keep them in, as
the baseline:

- `[Worth challenging]` sections wait for Phase 3.5.
- `indirect` Memorabilia cases wait for Phase 4.7, analogy edges.
- `superseded` cases wait for Phase 4.4.

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
