import { useEffect, useState } from 'react';
import { ipc, onEngineRestartState } from '@/lib/ipc';
import { useConfigDraft } from './useConfigDraft';
import type { EngineRestartState, LocalModelSettings } from '@/lib/types';

/** §6.2/D6. Names match `bigtiny_rust::provider::presets::NAMES` — the daemon
    resolves the string, so a typo here silently means "no preset". */
export const SAMPLING_PRESETS = ['precise', 'balanced', 'creative'] as const;

const CACHE_TYPES = ['f16', 'q8_0', 'q4_0', 'q4_1', 'q5_0', 'q5_1'] as const;
const BACKENDS = ['auto', 'cuda', 'vulkan', 'cpu'] as const;

/** Context sizes offered as detents. Free-typing an arbitrary `n_ctx` is a
    good way to exhaust memory on a load that then fails opaquely, and every
    value here is a power of two the KV cache is sized against anyway. */
const CTX_CHOICES = [2048, 4096, 8192, 16384, 32768] as const;

/** Human summary of what a restart is waiting for. Exported for testing —
    this repo has no component-render tests, so display logic is only
    reachable this way. */
export function restartChipLabel(s: EngineRestartState): string | null {
  if (!s.reload_required) return null;
  return s.restart_pending
    ? 'Restarting when the current reply finishes…'
    : 'Applying engine settings…';
}

/** Engine knobs, presets and the model card (docs/ANDROID.md §6.1–§6.4).
 *
 * Every setting here is load-time: saving one restarts the daemon, either
 * immediately or once the in-flight generation finishes. The chip says which
 * — never a modal, per §6.4. */
export function LocalEngineSettings() {
  const { draft, update, save, saved, error } = useConfigDraft();
  const [restart, setRestart] = useState<EngineRestartState>({
    reload_required: false,
    restart_pending: false,
  });

  useEffect(() => {
    void ipc
      .getEngineRestartState()
      .then(setRestart)
      .catch(() => {});
    const un = onEngineRestartState(setRestart);
    return () => void un.then((fn) => fn());
  }, []);

  if (!draft) return <p className="muted">Loading…</p>;

  const local = draft.local;
  const setLocal = (patch: Partial<LocalModelSettings>) =>
    update({ local: { ...local, ...patch } });

  const chip = restartChipLabel(restart);

  return (
    <>
      <h2>Engine</h2>
      <p className="muted">
        These take effect on a daemon restart, which Kitty does for you — immediately if nothing is
        generating, otherwise once the current reply finishes.
      </p>

      {chip && <p className="muted">{chip}</p>}
      {error && <div className="chat-error">{error}</div>}

      <label className="field">
        <span>Context window</span>
        <select value={local.n_ctx} onChange={(e) => setLocal({ n_ctx: Number(e.target.value) })}>
          {CTX_CHOICES.map((n) => (
            <option key={n} value={n}>
              {n.toLocaleString()} tokens
            </option>
          ))}
        </select>
        <small className="muted">
          Bigger remembers more per turn and uses more memory. Clamped to whatever the model was
          actually trained on.
        </small>
      </label>

      <label className="field">
        <span>Compute backend</span>
        <select value={local.backend} onChange={(e) => setLocal({ backend: e.target.value })}>
          {BACKENDS.map((b) => (
            <option key={b} value={b}>
              {b === 'auto' ? 'Automatic' : b.toUpperCase()}
            </option>
          ))}
        </select>
        <small className="muted">
          This build is CPU-only — GPU backends aren&apos;t compiled in yet, so anything other than
          CPU falls back to it.
        </small>
      </label>

      <label className="field">
        <span>GPU layers</span>
        <input
          type="number"
          value={local.n_gpu_layers}
          onChange={(e) => setLocal({ n_gpu_layers: Number(e.target.value) })}
        />
        <small className="muted">
          −1 offloads everything to the GPU; 0 keeps it all on the CPU.
        </small>
      </label>

      <label className="field">
        <span>CPU threads</span>
        <input
          type="number"
          min={0}
          value={local.n_threads}
          onChange={(e) => setLocal({ n_threads: Number(e.target.value) })}
        />
        <small className="muted">0 lets llama.cpp pick from your core count.</small>
      </label>

      <label className="field">
        <span>Batch size</span>
        <input
          type="number"
          min={1}
          value={local.n_batch}
          onChange={(e) => setLocal({ n_batch: Number(e.target.value) })}
        />
      </label>

      <div className="row">
        <label className="field">
          <span>KV cache (keys)</span>
          <select
            value={local.cache_type_k}
            onChange={(e) => setLocal({ cache_type_k: e.target.value })}
          >
            {CACHE_TYPES.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          <span>KV cache (values)</span>
          <select
            value={local.cache_type_v}
            onChange={(e) => setLocal({ cache_type_v: e.target.value })}
          >
            {CACHE_TYPES.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
        </label>
      </div>
      <p className="muted">
        Quantising the KV cache saves memory on long contexts. f16 is the safe default and the only
        one guaranteed on every backend.
      </p>

      <h2>Embeddings</h2>
      <label className="field">
        <span>Embedding context</span>
        <input
          type="number"
          min={64}
          value={local.embed_n_ctx}
          onChange={(e) => setLocal({ embed_n_ctx: Number(e.target.value) })}
        />
        <small className="muted">Beliefs are short, so this stays small — 512 is ample.</small>
      </label>

      <label className="field">
        <span>Pooling</span>
        <select
          value={local.embed_pooling}
          onChange={(e) => setLocal({ embed_pooling: e.target.value })}
        >
          <option value="last">Last token (Qwen3-Embedding and other causal models)</option>
          <option value="mean">Mean (BERT-style: bge, gte, nomic)</option>
          <option value="cls">CLS token</option>
        </select>
        <small className="muted">
          Getting this wrong doesn&apos;t error — it quietly degrades recall. Match it to the model.
        </small>
      </label>

      <div className="row">
        <button className="primary" onClick={() => void save()}>
          Save
        </button>
        {saved && <span className="muted">Saved.</span>}
      </div>
    </>
  );
}
