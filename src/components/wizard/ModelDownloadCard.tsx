import { useEffect, useState } from 'react';
import { ipc, onModelProgress } from '@/lib/ipc';
import { CURATED_MODELS, type CuratedModel } from '@/lib/curated_models';
import type { DownloadProgress } from '@/lib/types';

/** Fixed per role, so the progress subscription is registered before the
    download starts rather than racing it. */
export const downloadIdFor = (role: CuratedModel['role']) => `wizard-${role}-model`;

/** One curated GGUF: its name, size, a Download button, and progress.
 *
 * Split out of `ModelDownloadStep` so a step can host **more than one**.
 * Android's first run downloads the summarizer and the embedder together as
 * "support models" (§8.3) — two of these in one step — while desktop still
 * shows one per step. The alternative was a second copy of the progress
 * subscription and the installed-check, which would drift. */
export function ModelDownloadCard({
  role,
  onInstalledChange,
}: {
  role: CuratedModel['role'];
  /** Lets the hosting step gate "Continue" on what is actually on disk. */
  onInstalledChange?: (installed: boolean) => void;
}) {
  const model = CURATED_MODELS.find((m) => m.role === role);
  const [progress, setProgress] = useState<DownloadProgress | null>(null);
  const [installed, setInstalled] = useState(false);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  // A HuggingFace token for a gated repo, held only in memory for this render.
  // Never persisted, never sent anywhere but the one download request.
  const [token, setToken] = useState('');

  useEffect(() => {
    const refresh = () => {
      if (!model) return;
      void ipc
        .listLocalModels()
        .then((ms) => {
          const have = ms.some((m) => m.file.toLowerCase() === model.file.toLowerCase());
          setInstalled(have);
          onInstalledChange?.(have);
        })
        .catch(() => {});
    };
    refresh();
    const un = onModelProgress((p) => {
      if (p.download_id !== downloadIdFor(role)) return;
      setProgress(p);
      if (p.done) {
        setBusy(false);
        if (p.error) setError(p.error);
        else refresh();
      }
    });
    return () => void un.then((fn) => fn());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [role]);

  if (!model) {
    return <p className="error">No {role} model is configured — this is a bug, not a setting.</p>;
  }

  const start = async () => {
    setError('');
    setBusy(true);
    try {
      await ipc.downloadModel(
        model.repo,
        model.file,
        undefined,
        downloadIdFor(role),
        model.gated ? token.trim() || undefined : undefined,
      );
      // Drop the token from memory the moment the request is on its way — it
      // isn't needed again, and the resume path re-prompts if it ever is.
      setToken('');
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  const pct =
    progress && progress.total ? Math.round((progress.received / progress.total) * 100) : null;

  return (
    <>
      <div className="model-row">
        <div>
          <div className="model-name">{model.label}</div>
          <div className="muted">
            {model.blurb} · {model.size_gb} GB
          </div>
        </div>
        {installed ? (
          <span className="muted">Downloaded</span>
        ) : (
          <button className="primary" disabled={busy} onClick={() => void start()}>
            {busy ? 'Downloading…' : 'Download'}
          </button>
        )}
      </div>

      {model.gated && !installed && (
        <div className="gated-token">
          <label className="muted" htmlFor={`hf-token-${role}`}>
            This model is under the Gemma license. Accept it on the model page, then paste a{' '}
            <a href={`https://huggingface.co/${model.repo}`} target="_blank" rel="noreferrer">
              HuggingFace access token
            </a>{' '}
            (read scope). It is used only for this download and never stored.
          </label>
          <input
            id={`hf-token-${role}`}
            type="password"
            autoComplete="off"
            placeholder="hf_…"
            value={token}
            disabled={busy}
            onChange={(e) => setToken(e.target.value)}
          />
        </div>
      )}

      {progress && !installed && (
        <div className="pull-row">
          <div className="pull-head">
            <span>{progress.model}</span>
            <span className="muted">{progress.error ?? (pct !== null ? `${pct}%` : '…')}</span>
          </div>
          <div className="progress">
            <div className="progress-bar" style={{ width: pct !== null ? `${pct}%` : '30%' }} />
          </div>
        </div>
      )}

      {error && <p className="error">{error}</p>}
    </>
  );
}
