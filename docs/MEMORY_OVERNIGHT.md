# Overnight memory: selective intake, learning at night

Memorabilia used to ingest every document a turn touched — each paste,
attachment and scraped page — chunk it into fixed 1 024-character windows,
embed it during the turn, and run LLM extraction on every chunk in a sweep every
60 s on whatever model the summarizer chain picked. Most of what it stored was
irrelevant, and the work competed with whatever the user was doing on the PC.

This document is the plan of record for the replacement: **be selective at
intake, and do the expensive work at night.** Adaptive-pathway's learning moves
to the night with it on desktop. Android is unchanged.

## Goal

Memorabilia is a continuous-learning appliance for the chat model: it should
hold durable, well-supported knowledge from after the model's training data
ends, and spend compute only on material that has shown it matters. Two
principles follow:

- **Err on the side of too selective.** A document that is never learned costs
  little (the chat that used it already has it); a memory full of filler costs
  every recall.
- **Relevance to the question that fetched a document is not required.** A
  paragraph can be worth keeping for some later use. What decides is whether
  the paragraph is durable and substantive (the d1 screen) and whether its
  topic keeps coming back (recurrence).

## Pipeline

```
DAY — at turn end, no local models               NIGHT — 00:00–05:00, cheapest first
────────────────────────────────────            ─────────────────────────────────────
harvest_turn → Engine::capture                   Engine::overnight_pass
  1. privacy check (regex), as before              1. drop captures unscreened > 3 days
  2. whole-document hash: seen / forgotten         2. screened captures → 600-word chunks
  3. split into paragraphs                             → embed → candidates (not recallable)
  4. free pre-filter                               3. embed the day's search queries
  5. d1 screen, per paragraph (if d1 is up)        4. recurrence: cluster, score, promote
  6. write kept paragraphs to the capture buffer   5. extraction on promoted clusters only
     (or the raw text, marked unscreened)          6. maintenance (decay, consolidation)
record this turn's search queries (demand)        pathway: learn sessions past watermark
                                                 stop at the window's end; resume next night
Engine::screen_pending — retries unscreened
captures whenever d1 is reachable again
```

### 1. Paragraphs and the free pre-filter

`learn::paragraphs` splits a document on blank lines and markdown blocks. A
markdown heading is not a paragraph: it becomes the context of the paragraphs
under it. Paragraphs under 200 characters merge into the next one; paragraphs
over 1 500 characters split at a sentence boundary. A markdown table or a list
stays whole.

The pre-filter drops, at no cost, paragraphs that are mostly links, mostly
digits, short-line runs (menus, tag clouds), reference-list entries, text that
repeats inside the document (headers, footers), and stock boilerplate (cookie
notices, "all rights reserved", "subscribe"). Every drop is logged with its
reason in the screen log (below).

### 2. The d1 paragraph screen

The decision model is asked, per paragraph, the questions in the app's
**question set**. Each question is a yes/no with its own keep threshold, and
may belong to an intake **lane**. A paragraph is kept when the shared
questions (no lane) and every question of at least one lane clear their
thresholds; the first such lane is the lane that kept it. A set without lanes
keeps a paragraph only when every question passes. The state the model sees is
the paragraph prefixed by the document's title, source and section heading, so
"it was rebuilt in 1987" can be judged.

The default set, `two-lane-v1` (uncalibrated starting point):

| name | lane | asks | keep at P(yes) ≥ |
|---|---|---|---|
| `current` | current | Does this passage report something recent that a well-read model trained a year ago would probably not know (a release, decision, change of status, new figure, event or announcement), stated concretely? | 0.60 |
| `durable` | durable | Will this passage still be informative in a few years, after the specific products, prices, people or events it mentions are no longer current? Yes for explanations of how or why things work, lasting history, definitions, durable relationships and reasoned arguments or interpretations; no for specs, prices, schedules and status updates. | 0.60 |
| `substantive` | (shared) | Is this passage substantive and well supported — concrete claims, mechanisms, evidence or reasoned argument — rather than marketing copy, filler, unsupported speculation or vague generalities? | 0.60 |

Why two lanes: the previous default (`durable-substantive-v1`) kept only
lasting material and so rejected exactly what brings a model up to date. A
document is `current` when any of its kept paragraphs came in through that
lane; its chunks are never filed as `static`, so they decay at the transient
rate and can be superseded (`proposition_relations`).

"Well supported" rather than "factual" on purpose: a 3B classifier cannot check
truth, and "factual" rejects reasoned argument — the failure that once made d1
reject the user's own op-eds.

**The question set is data, not code.** It lives in the app's decider
configuration (`paragraph_screen.questions`), so Kitty can send a different set
without a rebuild, and every verdict is logged with the set's id. See
*Testing question sets*.

**Early exit**: when the first 10 screened paragraphs of a document all fail,
the rest are dropped unscreened (spec sheets and listing pages tend to fail
from top to bottom).

**When d1 is unavailable** this use fails *closed*, the one exception to the
decider's fail-open rule: the capture is stored raw and marked `unscreened`;
`Engine::screen_pending` screens it, oldest first, the next time d1 answers;
an unscreened capture never reaches the overnight pass and is dropped after 3
days. When the app has no decision model, or has the memory screen switched
off, the pre-filter alone decides what is kept — the screen is an addition the
user opted into, not a precondition for learning.

### 3. The capture buffer and candidates

Only the kept paragraphs are stored (`captures.kept_text`), with the source and
the session and turn that brought them in. Overnight they are grouped into
chunks — consecutive kept paragraphs until the chunk has **at least two
paragraphs and at least 600 words**, whichever is longer; a short remainder
joins the chunk before it — and each chunk is embedded into the `candidates`
table. **Candidates are never recalled.** They live only in their own table,
outside the vector index recall searches.

### 4. Recurrence and promotion

Each night the waiting candidates are clustered (cosine ≥ 0.80). A cluster's
score:

| signal | points |
|---|---|
| each distinct chat the cluster's candidates came from | +1 |
| an existing memory chunk it closely matches (cos ≥ 0.80) | +1 |
| each further similar search (query cos ≥ 0.80 to one of the cluster's own turns' queries), counted once per turn, in a turn the cluster did not come from | +0.5 |

A cluster **promotes at 2**: two chats, or one chat plus two later similar
searches. Several queries the model fans out within one turn count once — the
signal is the user returning to a topic, not the model's search breadth.
Promotion writes **one candidate per distinct source** (most central first, up
to four, at least two) into the memory store as pending chunks; the rest of the
cluster is marked merged. Extraction therefore runs a few times per cluster,
not once per chunk of every document, and independent sources survive to
corroborate each other. Each pass reports `clusters_seen` and
`largest_cluster`, and logs a size histogram, to check that the raw-cosine
threshold does not chain unrelated topics together.

Candidates that never promote expire after 30 days, unextracted — their only
cost was an embedding.

### 5. Analogy edges

After extraction, each new claim gets a one-sentence **abstraction**: the
general principle it is an instance of, with its names, places and dates
removed. Claims whose abstractions are close (`analogy_abstract_min`) while
their literal claims are not (`analogy_claim_max`), from different sources,
are analogy candidates; the extraction model confirms each pair with a
one-line reason, and confirmed pairs get `RHYMES_WITH` edges. Recall's
spreading activation follows them from a direct hit, and the index shows up to
two such items under "Related by analogy", with the reason. Bounded per pass
(`analogy_claims_per_pass`, `analogy_pairs_per_pass`).

### 6. Adaptive-pathway

On desktop, with overnight learning on for the app, the turn-end hook keeps its
bookkeeping (the exchange counter the `[Where I'm unsure]` cadence reads) but
skips the learn pass, and the idle sweep learns nothing outside the window.
Inside it, the sweep learns every session past its watermark, in larger
batches. Beliefs from today's chats are recalled from tomorrow. Android keeps
turn-end learning: Kitty never switches overnight learning on there, and
memorabilia does not run on Android at all.

## The overnight window

Per app, in the engine (`/api/apps/me/overnight`), because the engine does the
work and runs while Kitty is closed:

- `enabled`, `start` and `end` (local time, `HH:MM`, may wrap midnight; default
  00:00–05:00), `pathway` (move pathway learning too).
- **Work happens** inside the window, or for a *catch-up* — the last full pass
  ended over 30 hours ago, the machine has been idle 20 minutes and is on mains
  power — or on **Process now** (`POST /api/apps/me/overnight/run`), which
  opens a one-hour window.
- **The user comes first.** Before each item the pass checks for keyboard or
  mouse input in the last 10 minutes; if there was some, it pauses until the
  machine is idle again.
- **Stopping**: the pass checks the clock between items and stops at the
  window's end mid-batch; everything left waits for the next night.
- With `enabled` off (or for an app that sends no settings), the same pass
  runs about once an hour instead — the selection pipeline is the same, only
  the timing differs. The recurrence step compares every waiting candidate
  with every other, which is why it does not run every minute.

### Waking the computer and letting it sleep again

- **Waking** is a per-user Windows Task Scheduler task that Kitty registers
  when the user turns on *Wake this computer* (off by default): daily at the
  window's start + 5 minutes, *Wake the computer to run this task*, AC power
  only. Its action is `kitty.exe --overnight-wake`, which returns immediately
  without opening a window. The daemon's own clock does the rest.
- **Staying awake**: while a pass runs the engine holds
  `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` on a dedicated
  thread (system awake, display off) and releases it when done.
- **Going back to sleep** needs no code. A timer wake is *unattended*: Windows
  returns to sleep about two minutes after nothing holds the system awake. The
  engine never calls `SetSuspendState`, which could put a machine to sleep under
  someone using it.
- **Caveats** Settings surfaces rather than fixes: *Allow wake timers* disabled
  in the power plan (often on battery, sometimes by the OEM); Modern Standby
  (S0ix) machines, where wake timers may not fire; and a machine that is shut
  down rather than asleep cannot be woken. Checks: `powercfg /a`,
  `powercfg /waketimers`, `powercfg /lastwake`.

## Testing question sets

1. **Collect.** Every screened paragraph is logged in memorabilia's
   `screen_log`: the text, its heading and source, the question set id, every
   question's P(yes), and the verdict (or the pre-filter reason). Settings →
   Memorabilia → *Save screened paragraphs for labelling* writes the recent log as JSONL, one
   object per paragraph with an empty `"keep": null` for the user to label.
2. **Label.** Set `"keep": true/false` on a sample (≈150 paragraphs is enough
   to separate two candidate sets).
3. **Score.** Write candidate sets to a JSON file (an array of
   `{ "id", "questions": [{ "name", "instructions", "keep_at" }] }`;
   `docs/screen-question-sets.example.json` has six to start from: the
   default, each of its questions alone, and three rewordings) and run
   ```bash
   D1_URL=http://<host>/d1 SCREEN_EVAL_LABELS=labels.jsonl SCREEN_EVAL_SETS=sets.json \
     cargo test --lib -- --ignored live_d1_screen_eval --nocapture
   ```
   in `BigTinyV2/daemon` (without `SCREEN_EVAL_SETS` it scores the default
   set). Each set asks its questions in one call per paragraph, exactly as
   production does, on the same text production sends. For each set it reports, at the set's own thresholds
   and at the best thresholds on a grid, the keep rate, precision, recall and
   F0.5 (precision weighted twice recall, matching "too selective rather than
   too inclusive").
4. **Deploy.** Put the winning set under `decider.paragraph_screen` in Kitty's
   `config.json` (same shape as one entry of the sets file); Kitty sends it to
   the engine on its next start or settings sync. No rebuild. The engine
   validates the set (unique names, instructions, `keep_at` in 0–1); one it
   rejects leaves the previous configuration in place, with the reason in
   Settings → Advanced's error log. Remove the key to go back to the default
   set.

## Settings → Memorabilia → Overnight learning

Shown whenever the embedding model is installed, Memorabilia on or off,
because it also decides when Adaptive Pathway learns (whose panel links to
it). On/off, the window, *Wake this computer* (with a warning when wake timers
are off), *Learn about you (Adaptive Pathway) overnight too*, the last run (when, how long,
captured / kept / promoted / expired, propositions written), the backlog
(waiting for the decision model, waiting for tonight, pending extraction), and
*Process now*.

## Not done yet

- A per-attachment *Remember this* mark that promotes a document without
  waiting for recurrence. The engine supports it (`captures.pinned`); the
  composer chip and the turn-text marker are a backlog item.
- Batching several paragraphs into one d1 call — cheaper, but its accuracy has
  to be measured with the eval first.
