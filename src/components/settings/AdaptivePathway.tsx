import { useEffect, useState } from 'react';
import {
  ipc,
  onAdaptivePathwayStatus,
  onAdaptivePathwayEmbeddingStatus,
  onPullProgress,
} from '@/lib/ipc';
import type { AdaptivePathwayStatus as ApStatus, EmbeddingModelStatus, PullProgress } from '@/lib/types';

const STATUS_LABEL: Record<ApStatus, string> = {
  disabled: 'Disabled',
  starting: 'Starting…',
  ok: 'Running',
  down: 'Not reachable',
};

/** Separate from `STATUS_LABEL` above: the sidecar can be `ok` while this is
    `downloading`/`missing` — degraded to the hashing fallback, not an outage. */
const EMBEDDING_STATUS_LABEL: Record<EmbeddingModelStatus, string> = {
  unknown: '',
  present: 'ready',
  downloading: 'downloading…',
  missing: 'not downloaded yet',
};

interface EnsembleWeights {
  ig_weight_min: number;
  ig_weight_max: number;
  pc_weight: number;
}

/** Settings for the Adaptive Pathway extension — learns tool-use preferences
    and suggests hints before the model picks a tool. On by default. One
    checkbox does both halves of enabling it: spawns/supervises the HTTP
    sidecar (a separate managed process, same as Ollama) *and* registers its
    MCP tools with BigTiny so the model can call `decide` mid-conversation —
    previously two separate controls (UX-simplification owner decision).
    Launch command/args/db path/port are power-user knobs, tucked behind an
    Advanced disclosure. */
export function AdaptivePathway() {
  const [enabled, setEnabled] = useState(false);
  const [launchCommand, setLaunchCommand] = useState('');
  const [launchArgs, setLaunchArgs] = useState('');
  const [dbPath, setDbPath] = useState('');
  const [port, setPort] = useState(8700);
  const [status, setStatus] = useState<ApStatus>('disabled');
  const [embeddingStatus, setEmbeddingStatus] = useState<EmbeddingModelStatus>('unknown');
  const [embeddingModel, setEmbeddingModel] = useState('');
  const [embeddingBackend, setEmbeddingBackend] = useState<'ollama' | 'hashing' | 'untried' | null>(
    null
  );
  const [installBusy, setInstallBusy] = useState(false);
  const [installError, setInstallError] = useState('');
  const [installProgress, setInstallProgress] = useState<PullProgress | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [weights, setWeights] = useState<EnsembleWeights | null>(null);
  const [advancedOpen, setAdvancedOpen] = useState(false);

  const load = async () => {
    const cfg = await ipc.getConfig();
    setEnabled(cfg.adaptive_pathway_enabled);
    setLaunchCommand(cfg.adaptive_pathway_launch_command);
    setLaunchArgs(cfg.adaptive_pathway_launch_args.join(' '));
    setDbPath(cfg.adaptive_pathway_db_path);
    setPort(cfg.adaptive_pathway_port);
    setEmbeddingModel(cfg.adaptive_pathway_embedding_model);
    await ipc
      .getAdaptivePathwayStatus()
      .then(setStatus)
      .catch(() => {});
    await ipc
      .getAdaptivePathwayEmbeddingStatus()
      .then(setEmbeddingStatus)
      .catch(() => {});
  };

  useEffect(() => {
    void load();
    const un = onAdaptivePathwayStatus((p) => setStatus(p.status));
    const unEmbedding = onAdaptivePathwayEmbeddingStatus((p) => setEmbeddingStatus(p.status));
    return () => {
      void un.then((fn) => fn());
      void unEmbedding.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    if (status !== 'ok') {
      setWeights(null);
      setEmbeddingBackend(null);
      return;
    }
    void ipc
      .adaptivePathwayGetState()
      .then((s) => {
        setWeights(s.ensemble_weights);
        setEmbeddingBackend(s.embedding?.backend ?? null);
      })
      .catch(() => {});
  }, [status]);

  useEffect(() => {
    const un = onPullProgress((p) => {
      if (p.model !== embeddingModel) return;
      setInstallProgress(p);
      if (p.done && !p.error) {
        void ipc
          .getAdaptivePathwayEmbeddingStatus()
          .then(setEmbeddingStatus)
          .catch(() => {});
      }
    });
    return () => void un.then((fn) => fn());
  }, [embeddingModel]);

  /** Settings' own "set up the learning model" action (Gap 2): checks
      Ollama, installs it if missing, ensures it's running, then pulls the
      pinned tag — same building blocks as `EmbeddingModelStep` in the
      wizard, just reachable after setup too (e.g. the user skipped it, or
      deleted the model later with `ollama rm`). */
  const installEmbeddingModel = async () => {
    setInstallBusy(true);
    setInstallError('');
    try {
      const det = await ipc.detectDependencies();
      if (!det.ollama.installed) {
        await ipc.installDependency('ollama');
        const deadline = Date.now() + 120_000;
        while (Date.now() < deadline) {
          await new Promise((r) => setTimeout(r, 2_000));
          const fresh = await ipc.detectDependencies();
          if (fresh.ollama.installed) break;
        }
      }
      await ipc.ensureOllamaRunning();
      await ipc.ollamaPullModel(embeddingModel);
    } catch (e) {
      setInstallError(String(e));
    } finally {
      setInstallBusy(false);
    }
  };

  const saveField = async (patch: {
    adaptive_pathway_launch_command?: string;
    adaptive_pathway_launch_args?: string[];
    adaptive_pathway_db_path?: string;
    adaptive_pathway_port?: number;
  }) => {
    const cfg = await ipc.getConfig();
    await ipc.setConfig({ ...cfg, ...patch });
  };

  /** Turning this on/off drives both the sidecar process and its BigTiny MCP
      server registration (the `decide`/`record_outcome` tools) together —
      the Rust command self-heals the MCP registration, so the frontend just
      flips the flag and re-reads status. */
  const setEnabledCombined = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      setEnabled(next);
      await ipc.setAdaptivePathwayEnabled(next);
      await ipc.getAdaptivePathwayStatus().then(setStatus);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const restart = async () => {
    setBusy(true);
    setError('');
    try {
      await ipc.restartAdaptivePathway();
      await ipc.getAdaptivePathwayStatus().then(setStatus);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const commitWeights = async (next: EnsembleWeights) => {
    try {
      const result = await ipc.adaptivePathwayUpdateEnsembleWeights(
        next.ig_weight_min,
        next.ig_weight_max,
        next.pc_weight
      );
      setWeights(result);
    } catch (e) {
      setError(String(e));
    }
  };

  const statusDotClass = status === 'ok' ? 'ok' : status === 'down' ? 'bad' : 'warn';

  return (
    <section className="settings-section">
      <h1>Adaptive Pathway</h1>
      <p className="muted">
        Learns your tool-use preferences and suggests hints before the model picks a tool.
      </p>
      {error && <div className="chat-error">{error}</div>}

      <label className="check">
        <input
          type="checkbox"
          checked={enabled}
          disabled={busy}
          onChange={(e) => void setEnabledCombined(e.target.checked)}
        />
        <span>Enable Adaptive Pathway</span>
      </label>
      <small className="muted">
        Starts the background process that learns your preferences and registers its tools so the
        model can actually suggest hints. Requires{' '}
        <code>pip install adaptive-pathway[sidecar]</code> — if it isn&apos;t installed, this just
        stays &quot;Not reachable&quot; below, no error spam.
      </small>

      <div className="row" style={{ alignItems: 'center' }}>
        <span className={`status-dot ${statusDotClass}`} />
        <span>{STATUS_LABEL[status]}</span>
        {status !== 'disabled' && embeddingStatus !== 'unknown' && (
          <span className="muted">· learning model {EMBEDDING_STATUS_LABEL[embeddingStatus]}</span>
        )}
        {embeddingBackend && embeddingBackend !== 'untried' && (
          <span className="muted">
            · {embeddingBackend === 'ollama' ? 'semantic embeddings active' : 'using fallback vectors'}
          </span>
        )}
        <button disabled={busy || status === 'disabled'} onClick={() => void restart()}>
          Restart
        </button>
      </div>

      {status !== 'disabled' && embeddingStatus === 'missing' && (
        <div className="dep-row" style={{ flexDirection: 'column', alignItems: 'stretch', gap: 8 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <span className="muted">
              The shared learning model ({embeddingModel}) isn&apos;t downloaded yet — suggestions
              still work, just with less precision.
            </span>
            <button disabled={installBusy} onClick={() => void installEmbeddingModel()}>
              {installBusy ? 'Setting up…' : 'Set up learning model'}
            </button>
          </div>
          {installProgress && (
            <div className="pull-row">
              <div className="pull-head">
                <span>{installProgress.model}</span>
                <span className="muted">
                  {installProgress.error ? `error: ${installProgress.error}` : installProgress.status}
                </span>
              </div>
              <div className="progress">
                <div
                  className="progress-bar"
                  style={{
                    width: installProgress.done
                      ? '100%'
                      : installProgress.total && installProgress.completed
                        ? `${Math.round((installProgress.completed / installProgress.total) * 100)}%`
                        : '30%',
                  }}
                />
              </div>
            </div>
          )}
          {installError && <p className="muted">Couldn&apos;t set up automatically: {installError}</p>}
        </div>
      )}

      <button
        type="button"
        className="disclosure-toggle"
        onClick={() => setAdvancedOpen((o) => !o)}
      >
        {advancedOpen ? '▾' : '▸'} Advanced
      </button>
      {/* Explicit conditional render, not native <details> collapse — this
          WebView2/Chromium build doesn't actually hide non-open <details>
          content (confirmed live via Providers.tsx's identical disclosure),
          so visibility can't be left to CSS. */}
      {advancedOpen && (
        <div>
          <p className="muted">
            Launch command, extra args, database path, and port — the sidecar and the MCP tools
            both point at the same database path so they see the same learned data.
          </p>
          <div className="field">
            <span>Launch command</span>
            <input
              value={launchCommand}
              onChange={(e) => setLaunchCommand(e.target.value)}
              onBlur={() => void saveField({ adaptive_pathway_launch_command: launchCommand })}
            />
          </div>
          <div className="field">
            <span>Extra launch args (space-separated, e.g. --config-path)</span>
            <input
              value={launchArgs}
              onChange={(e) => setLaunchArgs(e.target.value)}
              onBlur={() =>
                void saveField({
                  adaptive_pathway_launch_args: launchArgs.split(/\s+/).filter(Boolean),
                })
              }
            />
          </div>
          <div className="field">
            <span>Database path</span>
            <input
              value={dbPath}
              onChange={(e) => setDbPath(e.target.value)}
              onBlur={() => void saveField({ adaptive_pathway_db_path: dbPath })}
            />
          </div>
          <div className="field">
            <span>Port</span>
            <input
              type="number"
              value={port}
              onChange={(e) => setPort(Number(e.target.value))}
              onBlur={() => void saveField({ adaptive_pathway_port: port })}
            />
          </div>
        </div>
      )}

      <h2>Insights (advanced)</h2>
      <p className="muted">
        Live, per-process tuning — no restart needed. These shift how much weight two of the
        extension&apos;s models get when blending a suggestion; leave them alone unless you have a
        specific reason to adjust.
      </p>
      {!weights && <p className="muted">Enable Adaptive Pathway to configure these.</p>}
      {weights && (
        <>
          <div className="field param-slider">
            <span>
              IG weight floor — the Information-Gain model&apos;s minimum influence, even in
              familiar territory
            </span>
            <small className="muted">
              Keeps Kitty exploring a little, even when it&apos;s confident.
            </small>
            <div className="row">
              <input
                type="range"
                min={0}
                max={0.3}
                step={0.01}
                value={weights.ig_weight_min}
                onChange={(e) => setWeights({ ...weights, ig_weight_min: Number(e.target.value) })}
                onMouseUp={() => void commitWeights(weights)}
                onTouchEnd={() => void commitWeights(weights)}
              />
              <span className="status-badge">{weights.ig_weight_min.toFixed(2)}</span>
            </div>
          </div>
          <div className="field param-slider">
            <span>
              IG weight ceiling — the Information-Gain model&apos;s maximum influence, when plateau
              risk is highest
            </span>
            <small className="muted">
              Caps how far Kitty leans into exploring when it&apos;s unsure what to suggest.
            </small>
            <div className="row">
              <input
                type="range"
                min={0.3}
                max={0.7}
                step={0.01}
                value={weights.ig_weight_max}
                onChange={(e) => setWeights({ ...weights, ig_weight_max: Number(e.target.value) })}
                onMouseUp={() => void commitWeights(weights)}
                onTouchEnd={() => void commitWeights(weights)}
              />
              <span className="status-badge">{weights.ig_weight_max.toFixed(2)}</span>
            </div>
          </div>
          <div className="field param-slider">
            <span>
              Paradigm Challenge weight — raise to strengthen the bias counterweight against
              tunnel-visioned suggestions
            </span>
            <small className="muted">
              Keeps Kitty from getting stuck suggesting the same kind of thing.
            </small>
            <div className="row">
              <input
                type="range"
                min={0}
                max={0.25}
                step={0.01}
                value={weights.pc_weight}
                onChange={(e) => setWeights({ ...weights, pc_weight: Number(e.target.value) })}
                onMouseUp={() => void commitWeights(weights)}
                onTouchEnd={() => void commitWeights(weights)}
              />
              <span className="status-badge">{weights.pc_weight.toFixed(2)}</span>
            </div>
          </div>
        </>
      )}
    </section>
  );
}
