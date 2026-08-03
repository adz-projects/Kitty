import { useEffect, useRef, useState } from 'react';
import { ipc, onPullProgress } from '@/lib/ipc';
import type { Config, Detection, PullProgress } from '@/lib/types';

/** How long to keep polling after handing off to Ollama's installer before
    giving up (mirrors DetectStep — Ollama has no "installer finished" signal
    Kitty can observe directly). */
const INSTALL_POLL_TIMEOUT_MS = 120_000;
const INSTALL_POLL_INTERVAL_MS = 2_000;

/** Best-effort first-run pull of the shared embedding model, shown on BOTH
    wizard paths (local and api-key) when adaptive-pathway is enabled — an
    API-key chat user still needs Ollama purely for adaptive-pathway's
    embeddings (they're local-Ollama-only regardless of chat provider, for
    cross-compatibility). Never blocks `complete_setup`: a runtime auto-pull
    on every launch (`lifecycle::ensure_embedding_model`) is the actual
    guarantee — this step is just a head start so the download doesn't happen
    invisibly in the background on first use. */
export function EmbeddingModelStep({
  cfg,
  onBack,
  onNext,
  onSkip,
}: {
  cfg: Config;
  onBack: () => void;
  onNext: () => void;
  onSkip: () => void;
}) {
  const model = cfg.adaptive_pathway_embedding_model;
  const [det, setDet] = useState<Detection | null>(null);
  const [installed, setInstalled] = useState<string[]>([]);
  const [progress, setProgress] = useState<PullProgress | null>(null);
  const [busy, setBusy] = useState(false);
  const [waitingForInstaller, setWaitingForInstaller] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancelled = useRef(false);

  const refresh = async () => {
    const [d, models] = await Promise.all([
      ipc.detectDependencies(),
      ipc.ollamaListModels().catch(() => []),
    ]);
    setDet(d);
    setInstalled(models.map((m) => m.name));
  };

  useEffect(() => {
    void refresh();
    const un = onPullProgress((p) => {
      if (p.model !== model) return;
      setProgress(p);
      if (p.done && !p.error) void refresh();
    });
    return () => {
      cancelled.current = true;
      void un.then((fn) => fn());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const have = installed.includes(model);
  const pct =
    progress?.total && progress?.completed
      ? Math.round((progress.completed / progress.total) * 100)
      : null;

  const installOllama = async () => {
    setBusy(true);
    setError(null);
    try {
      await ipc.installDependency('ollama');
      setWaitingForInstaller(true);
      const deadline = Date.now() + INSTALL_POLL_TIMEOUT_MS;
      let installed = false;
      while (Date.now() < deadline && !cancelled.current) {
        await new Promise((r) => setTimeout(r, INSTALL_POLL_INTERVAL_MS));
        const fresh = await ipc.detectDependencies();
        if (fresh.ollama.installed) {
          setDet(fresh);
          installed = true;
          break;
        }
      }
      setWaitingForInstaller(false);
      if (!installed) {
        if (!cancelled.current) {
          setError(
            "Didn't detect a finished install after 2 minutes. Finish the installer window, then try again."
          );
        }
        return;
      }
      // Installing the binary doesn't start it — `ollamaPullModel` hits
      // Ollama's own HTTP API directly and fails outright if nothing is
      // listening yet.
      await ipc.ensureOllamaRunning();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const [pulling, setPulling] = useState(false);
  const startDownload = async () => {
    setError(null);
    setPulling(true);
    try {
      // Covers "Ollama is installed but not currently running" — e.g. it was
      // installed in a previous session and never auto-started because
      // nothing else needed it yet.
      await ipc.ensureOllamaRunning();
      await ipc.ollamaPullModel(model);
    } catch (e) {
      setError(String(e));
    } finally {
      setPulling(false);
    }
  };

  return (
    <section className="wizard-panel">
      <h1>Download the shared learning model</h1>
      <p className="muted">
        Adaptive Pathway (Kitty's suggestion-learning feature) uses a small local model to
        understand what each conversation is about. This runs on your machine via Ollama, regardless
        of which chat provider you use — it's never sent anywhere.
      </p>

      {!det && <p className="muted">Checking what's already installed…</p>}

      {det && !det.ollama.installed && (
        <div className="dep-row" style={{ flexDirection: 'column', alignItems: 'stretch', gap: 8 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <div>
              <strong>Ollama</strong> <span className="status-badge">not found</span>
            </div>
            <button disabled={busy} onClick={() => void installOllama()}>
              {waitingForInstaller
                ? 'Waiting for install to finish…'
                : busy
                  ? 'Installing…'
                  : 'Install Ollama'}
            </button>
          </div>
          {error && <p className="muted">Couldn't install automatically: {error}</p>}
        </div>
      )}

      {det && det.ollama.installed && (
        <div className="starter-list">
          <div className="starter selected">
            <div>
              <div>
                <strong>{model}</strong>
                {have && <span className="status-badge">installed</span>}
              </div>
            </div>
          </div>
        </div>
      )}

      {det && det.ollama.installed && error && <p className="muted">Couldn't download: {error}</p>}

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

      <p className="muted" style={{ fontSize: 12 }}>
        This never blocks setup — if you skip it, Kitty pulls it automatically the next time it
        starts, and suggestions keep working (with slightly less precision) in the meantime.
      </p>

      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        <button className="link" onClick={onSkip}>
          Skip for now
        </button>
        {det && det.ollama.installed && !have && (
          <button className="primary" disabled={pulling} onClick={() => void startDownload()}>
            {pulling ? 'Downloading…' : 'Download'}
          </button>
        )}
        {det && det.ollama.installed && have && (
          <button className="primary" onClick={onNext}>
            Continue →
          </button>
        )}
      </div>
    </section>
  );
}
