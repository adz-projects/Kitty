import { useEffect, useState } from 'react';
import { ipc, onModelProgress } from '@/lib/ipc';
import { CURATED_MODELS, type CuratedModel } from '@/lib/curated_models';
import type { DownloadProgress } from '@/lib/types';

/** Fixed per role, so the progress subscription is registered before the
    download starts rather than racing it. */
const downloadIdFor = (role: CuratedModel['role']) => `wizard-${role}-model`;

interface Props {
  role: CuratedModel['role'];
  title: string;
  blurb: string;
  /** Shown under the button when the user can safely move on without this. */
  skipNote?: string;
  onBack: () => void;
  onNext: () => void;
  onSkip: () => void;
}

/** Download one curated GGUF. Used for both the chat model and the memory
    engine's embedding model — same mechanics, different copy, so one
    component rather than two that drift.
 *
 * Replaces the wizard's old detect-and-install-Ollama pair: there is no
 * third-party installer to run any more, no UAC prompt, and no version to
 * detect. Just a file. */
export function ModelDownloadStep({ role, title, blurb, skipNote, onBack, onNext, onSkip }: Props) {
  const model = CURATED_MODELS.find((m) => m.role === role);
  const [progress, setProgress] = useState<DownloadProgress | null>(null);
  const [installed, setInstalled] = useState(false);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  const refresh = () => {
    if (!model) return;
    void ipc
      .listLocalModels()
      .then((ms) => setInstalled(ms.some((m) => m.file.toLowerCase() === model.file.toLowerCase())))
      .catch(() => {});
  };

  useEffect(() => {
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
    return (
      <div className="wizard-body">
        <h1>{title}</h1>
        <p className="error">No {role} model is configured — this is a bug, not a setting.</p>
        <div className="row">
          <button onClick={onBack}>Back</button>
          <button onClick={onSkip}>Skip</button>
        </div>
      </div>
    );
  }

  const start = async () => {
    setError('');
    setBusy(true);
    try {
      await ipc.downloadModel(model.repo, model.file, undefined, downloadIdFor(role));
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  const pct =
    progress && progress.total ? Math.round((progress.received / progress.total) * 100) : null;

  return (
    <div className="wizard-body">
      <h1>{title}</h1>
      <p className="muted">{blurb}</p>

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
      {skipNote && !installed && <p className="muted">{skipNote}</p>}

      <div className="row">
        <button onClick={onBack}>Back</button>
        {installed ? (
          <button className="primary" onClick={onNext}>
            Continue
          </button>
        ) : (
          <button onClick={onSkip}>Skip for now</button>
        )}
      </div>
    </div>
  );
}
