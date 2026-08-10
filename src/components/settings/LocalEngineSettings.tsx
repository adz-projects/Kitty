import { useEffect, useState } from 'react';
import { ipc, onEngineRestartState } from '@/lib/ipc';
import { useConfigDraft } from './useConfigDraft';
import type {
  EngineRestartState,
  LocalEngineStatus,
  LocalModelSettings,
  LocalSlotStatus,
  SelectedBackend,
} from '@/lib/types';

/** §6.2/D6. Names match `bigtiny_rust::provider::presets::NAMES` — the daemon
    resolves the string, so a typo here silently means "no preset". */
export const SAMPLING_PRESETS = ['precise', 'balanced', 'creative'] as const;

const CACHE_TYPES = ['f16', 'q8_0', 'q4_0', 'q4_1', 'q5_0', 'q5_1'] as const;
const BACKENDS = ['auto', 'cuda', 'vulkan', 'cpu'] as const;

/** Context sizes offered as detents. Free-typing an arbitrary `n_ctx` is a
    good way to exhaust memory on a load that then fails opaquely, and every
    value here is a power of two the KV cache is sized against anyway.

    `0` is the daemon's "automatic" sentinel — it hands the decision to
    llama.cpp's `fit_params`, which sizes the context against measured device
    memory. On a CPU-only machine nothing is fitted and it resolves to 4096
    (`engine.rs`'s `AUTO_N_CTX_FALLBACK`); the label says so rather than
    implying a measurement that didn't happen. */
const CTX_CHOICES = [0, 2048, 4096, 8192, 16384, 32768] as const;

function ctxLabel(n: number): string {
  return n === 0 ? 'Automatic (fit to device memory)' : `${n.toLocaleString()} tokens`;
}

/** How a slot's offload actually resolved, as one line.

    The interesting case is the middle one. "Automatic" GPU layers can land on
    a *partial* offload — 12 of 16 layers on the card, the rest on the CPU —
    which is neither "on the GPU" nor "on the CPU", and is exactly the outcome
    a user needs to see to understand why generation is slower than they
    expected. Saying just "Vulkan" there would hide it.

    Exported for testing — this repo has no component-render tests, so display
    logic is only reachable this way. */
export function offloadSummary(slot: LocalSlotStatus, totalLayers?: number | null): string {
  if (!slot.loaded) return 'not loaded';
  const backend = slot.backend?.backend ?? 'cpu';
  if (backend === 'cpu') return 'CPU';
  const n = slot.n_gpu_layers;
  const device = slot.backend?.device ?? backend.toUpperCase();
  if (n === 0) return `CPU (${device} available but nothing offloaded)`;
  // A negative count is llama.cpp's "all layers" sentinel surviving to the
  // status payload — only possible when fitting was skipped or failed.
  if (n == null || n < 0) return `${device}, all layers`;
  return totalLayers ? `${device}, ${n}/${totalLayers} layers` : `${device}, ${n} layers`;
}

/** VRAM as "3.2 / 8.0 GB free", or null on CPU where both figures are 0 and
    printing "0 / 0 GB" would read as a broken device rather than an absent
    one. */
export function vramSummary(b: SelectedBackend | null | undefined): string | null {
  if (!b || !b.memory_total) return null;
  const gb = (n: number) => (n / 1e9).toFixed(1);
  return `${gb(b.memory_free)} / ${gb(b.memory_total)} GB free`;
}

/** Human summary of what a restart is waiting for. Exported for testing —
    this repo has no component-render tests, so display logic is only
    reachable this way. */
export function restartChipLabel(s: EngineRestartState): string | null {
  if (!s.reload_required) return null;
  return s.restart_pending
    ? 'Restarting when the current reply finishes…'
    : 'Applying engine settings…';
}

/** What the engine actually resolved to, as opposed to what was asked for.
 *
 * Every setting above is a *request*: the backend can fall back to CPU,
 * `n_gpu_layers` can be fitted to what the card has room for, and `n_ctx` can
 * be clamped to what the model was trained on. Without this card those three
 * settings are unfalsifiable — the user changes them and has no way to learn
 * whether anything happened. */
function EngineStatusCard({ status }: { status: LocalEngineStatus }) {
  const selected = status.backend_selected;
  const vram = vramSummary(selected);
  const loaded = status.slots.filter((s) => s.loaded || s.error);

  return (
    <div className="model-row" style={{ display: 'block' }}>
      <div className="model-name">
        {selected?.device ?? (selected?.backend ?? 'cpu').toUpperCase()}
        {vram && <span className="muted"> · {vram}</span>}
      </div>
      {loaded.length === 0 ? (
        <div className="muted">No model resident yet.</div>
      ) : (
        loaded.map((s) => (
          <div key={s.kind} className="muted">
            {s.kind}: {s.error ?? offloadSummary(s)}
            {s.n_ctx ? ` · ${s.n_ctx.toLocaleString()} ctx` : ''}
          </div>
        ))
      )}
    </div>
  );
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
  const [status, setStatus] = useState<LocalEngineStatus | null>(null);

  useEffect(() => {
    void ipc
      .getEngineRestartState()
      .then(setRestart)
      .catch(() => {});
    const un = onEngineRestartState(setRestart);
    return () => void un.then((fn) => fn());
  }, []);

  // Re-read after a restart settles: the whole point of the card is to show
  // what the *resident* model is on, so it has to change when the model does.
  useEffect(() => {
    void ipc
      .getLocalEngineStatus()
      .then(setStatus)
      .catch(() => setStatus(null));
  }, [restart.reload_required, restart.restart_pending]);

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

      {status && <EngineStatusCard status={status} />}

      <label className="field">
        <span>Context window</span>
        <select value={local.n_ctx} onChange={(e) => setLocal({ n_ctx: Number(e.target.value) })}>
          {CTX_CHOICES.map((n) => (
            <option key={n} value={n}>
              {ctxLabel(n)}
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
          Vulkan is compiled in and covers NVIDIA, AMD and Intel, including integrated graphics.
          CUDA isn&apos;t — picking a backend your machine can&apos;t provide falls back to the CPU
          rather than failing.
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
          −1 lets llama.cpp fit as many layers as the GPU has room for; 0 keeps it all on the CPU. A
          positive number pins that many layers and skips fitting.
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
