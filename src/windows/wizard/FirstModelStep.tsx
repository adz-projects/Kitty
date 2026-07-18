import { useEffect, useState } from 'react';
import { ipc, onPullProgress } from '@/lib/ipc';
import type { PullProgress } from '@/lib/types';
import { STARTER_MODELS } from '@/lib/starter_models';

export function FirstModelStep({
  onBack,
  onNext,
  onSkip,
}: {
  onBack: () => void;
  onNext: () => void;
  onSkip: () => void;
}) {
  const [selected, setSelected] = useState(STARTER_MODELS[0].tag);
  const [progress, setProgress] = useState<PullProgress | null>(null);
  const [installed, setInstalled] = useState<string[]>([]);

  useEffect(() => {
    void ipc.ollamaListModels().then((m) => setInstalled(m.map((x) => x.name)));
    const un = onPullProgress((p) => {
      setProgress(p);
      if (p.done && !p.error)
        void ipc.ollamaListModels().then((m) => setInstalled(m.map((x) => x.name)));
    });
    return () => void un.then((fn) => fn());
  }, []);

  const have = installed.includes(selected);
  const pct =
    progress?.total && progress?.completed
      ? Math.round((progress.completed / progress.total) * 100)
      : null;
  // Installed models that aren't in the curated starter list (Round-2 item 17) —
  // offered as ready-to-use options so the user needn't download a starter.
  const starterTags = new Set(STARTER_MODELS.map((m) => m.tag));
  const otherInstalled = installed.filter((name) => !starterTags.has(name));

  return (
    <section className="wizard-panel">
      <h1>Pick a starter model</h1>
      <p className="muted">All under 4B parameters — they run on modest hardware.</p>
      <div className="starter-list">
        {STARTER_MODELS.map((m) => (
          <label key={m.tag} className={`starter${selected === m.tag ? ' selected' : ''}`}>
            <input
              type="radio"
              name="model"
              checked={selected === m.tag}
              onChange={() => setSelected(m.tag)}
            />
            <div>
              <div>
                <strong>{m.label}</strong> <span className="muted">~{m.size_gb} GB</span>
                {installed.includes(m.tag) && <span className="status-badge">installed</span>}
              </div>
              <div className="muted" style={{ fontSize: 12 }}>
                {m.blurb}
              </div>
            </div>
          </label>
        ))}
      </div>

      {otherInstalled.length > 0 && (
        <>
          <p className="muted">Already installed on this machine:</p>
          <div className="starter-list">
            {otherInstalled.map((name) => (
              <label key={name} className={`starter${selected === name ? ' selected' : ''}`}>
                <input
                  type="radio"
                  name="model"
                  checked={selected === name}
                  onChange={() => setSelected(name)}
                />
                <div>
                  <strong>{name}</strong> <span className="status-badge">installed</span>
                </div>
              </label>
            ))}
          </div>
        </>
      )}

      {progress && (
        <div className="pull-row">
          <div className="pull-head">
            <span>{progress.model}</span>
            <span className="muted">
              {progress.error ? `error: ${progress.error}` : progress.status}
            </span>
          </div>
          <div className="progress">
            <div
              className="progress-bar"
              style={{ width: progress.done ? '100%' : pct != null ? `${pct}%` : '30%' }}
            />
          </div>
        </div>
      )}

      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        <button className="link" onClick={onSkip}>
          Skip for now
        </button>
        {have ? (
          <button className="primary" onClick={onNext}>
            Use this model →
          </button>
        ) : (
          <button className="primary" onClick={() => void ipc.ollamaPullModel(selected)}>
            Download
          </button>
        )}
      </div>
    </section>
  );
}
