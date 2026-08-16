import { useEffect, useState } from 'react';
import { ipc, onModelProgress, onModelsChanged } from '@/lib/ipc';
import { CURATED_MODELS } from '@/lib/curated_models';
import { isAndroid } from '@/lib/platform';
import type { DownloadProgress, LocalModel } from '@/lib/types';

/** Bytes as a short human string. Exported for testing — the repo has no
    component-render tests, so display logic is only reachable this way. */
export function humanBytes(bytes: number): string {
  if (!bytes) return '';
  const gb = bytes / 1e9;
  return gb >= 1 ? `${gb.toFixed(1)} GB` : `${(bytes / 1e6).toFixed(0)} MB`;
}

/** Warn below this much free space (§5.2). */
const LOW_SPACE_BYTES = 2e9;

/** Percentage complete, or null when the total isn't known yet. */
export function downloadPercent(p: DownloadProgress): number | null {
  if (!p.total || p.total <= 0) return null;
  return Math.min(100, Math.round((p.received / p.total) * 100));
}

/** Local GGUF management: what's installed, download with progress, delete. */
export function LocalModels() {
  const [models, setModels] = useState<LocalModel[]>([]);
  const [downloads, setDownloads] = useState<Record<string, DownloadProgress>>({});
  const [free, setFree] = useState<number | null>(null);
  const [error, setError] = useState('');
  // Per-file HuggingFace tokens for gated repos, held in memory only for this
  // render — never persisted, never sent anywhere but the download request.
  const [tokens, setTokens] = useState<Record<string, string>>({});

  const refresh = () => {
    void ipc
      .listLocalModels()
      .then(setModels)
      .catch((e) => setError(String(e)));
    void ipc
      .getModelsDiskFree()
      .then(setFree)
      .catch(() => setFree(null));
  };

  useEffect(() => {
    refresh();
    // This panel is conditionally rendered, so it mounts and unmounts every
    // time the user switches tabs — listeners and their cleanup timers must be
    // torn down or each revisit stacks another one on the last.
    const timers = new Set<ReturnType<typeof setTimeout>>();
    const unlistenProgress = onModelProgress((p) => {
      setDownloads((cur) => ({ ...cur, [p.download_id]: p }));
      if (p.done) {
        refresh();
        if (p.error) setError(p.error);
        const t = setTimeout(() => {
          timers.delete(t);
          setDownloads((cur) => {
            const next = { ...cur };
            delete next[p.download_id];
            return next;
          });
        }, 4000);
        timers.add(t);
      }
    });
    const unlistenChanged = onModelsChanged(refresh);
    return () => {
      void unlistenProgress.then((fn) => fn());
      void unlistenChanged.then((fn) => fn());
      timers.forEach(clearTimeout);
    };
  }, []);

  const installed = new Set(models.map((m) => m.file.toLowerCase()));

  const start = async (repo: string, file: string, gated?: boolean) => {
    setError('');
    try {
      await ipc.downloadModel(
        repo,
        file,
        undefined,
        undefined,
        gated ? tokens[file]?.trim() || undefined : undefined,
      );
      // Drop the token now the request is away.
      setTokens((cur) => {
        const next = { ...cur };
        delete next[file];
        return next;
      });
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (m: LocalModel) => {
    if (!confirm(`Delete ${m.id}? The file is removed from disk.`)) return;
    setError('');
    try {
      await ipc.deleteLocalModel(m.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const active = Object.values(downloads);

  return (
    <div className="settings-section">
      {/* The nav entry already says which section this is, and on Android it
          says something different ("Support Models"), so repeating a
          hardcoded title here would contradict it. */}
      <p className="muted">
        {isAndroid()
          ? 'Kitty runs these itself, in the background — they summarise long conversations and power memory. Chat runs through the provider you connect. Downloads come from Hugging Face.'
          : 'Models run inside Kitty — no separate server to install or keep running. Downloads come from Hugging Face.'}
      </p>

      {error && <div className="chat-error">{error}</div>}

      {free !== null && (
        <p className={free < LOW_SPACE_BYTES ? 'chat-error' : 'muted'}>
          {humanBytes(free)} free on this drive
          {free < LOW_SPACE_BYTES && ' — that may not be enough for another model.'}
        </p>
      )}

      {active.length > 0 && (
        <div className="model-list">
          {active.map((p) => {
            const pct = downloadPercent(p);
            return (
              <div key={p.download_id} className="pull-row">
                <div className="pull-head">
                  <span className="model-name">{p.model}</span>
                  <span className="muted">
                    {p.error
                      ? p.error
                      : p.done
                        ? 'Done'
                        : pct !== null
                          ? `${pct}%`
                          : humanBytes(p.received)}
                  </span>
                </div>
                <div className="progress">
                  {/* An unknown total gets a fixed-width bar rather than a
                      fake percentage — better to show motion than a number
                      we'd have to invent. */}
                  <div
                    className="progress-bar"
                    style={{ width: pct !== null ? `${pct}%` : '30%' }}
                  />
                </div>
              </div>
            );
          })}
        </div>
      )}

      <h2>Installed</h2>
      {models.length === 0 ? (
        <p className="muted">No models yet. Pick one below to get started.</p>
      ) : (
        <div className="model-list">
          {models.map((m) => (
            <div key={m.id} className="model-row">
              <div>
                <div className="model-name">{m.id}</div>
                <div className="muted">
                  {humanBytes(m.size_bytes)}
                  {m.info?.quantization && ` · ${m.info.quantization}`}
                  {m.info?.context_length &&
                    ` · ${Math.round(m.info.context_length / 1024)}k context`}
                </div>
              </div>
              <button onClick={() => void remove(m)}>Delete</button>
            </div>
          ))}
        </div>
      )}

      <h2>Available</h2>
      <div className="model-list">
        {CURATED_MODELS.map((c) => {
          const have = installed.has(c.file.toLowerCase());
          const busy = active.some((p) => p.model === c.file && !p.done);
          return (
            <div key={c.file} className="model-item">
              <div className="model-row">
                <div>
                  <div className="model-name">{c.label}</div>
                  <div className="muted">
                    {c.blurb} · {c.size_gb} GB
                  </div>
                </div>
                <button
                  disabled={have || busy}
                  onClick={() => void start(c.repo, c.file, c.gated)}
                  className={have ? undefined : 'primary'}
                >
                  {have ? 'Installed' : busy ? 'Downloading…' : 'Download'}
                </button>
              </div>
              {c.gated && !have && (
                <div className="gated-token">
                  <label className="muted" htmlFor={`hf-token-${c.file}`}>
                    Gemma-licensed: accept it on the{' '}
                    <a href={`https://huggingface.co/${c.repo}`} target="_blank" rel="noreferrer">
                      model page
                    </a>{' '}
                    and paste a HuggingFace token (read scope). Used only for this download, never
                    stored.
                  </label>
                  <input
                    id={`hf-token-${c.file}`}
                    type="password"
                    autoComplete="off"
                    placeholder="hf_…"
                    value={tokens[c.file] ?? ''}
                    disabled={busy}
                    onChange={(e) =>
                      setTokens((cur) => ({ ...cur, [c.file]: e.target.value }))
                    }
                  />
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
