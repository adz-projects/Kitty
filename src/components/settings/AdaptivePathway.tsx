import { useEffect, useState } from 'react';
import { ipc, onAdaptivePathwayEmbeddingStatus, onModelProgress } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import type { AdaptivePathwayMcpStatus, DownloadProgress, EmbeddingModelStatus } from '@/lib/types';
import { defaultFor } from '@/lib/curated_models';
import { BeliefBrowser } from './BeliefBrowser';
import { GraphHealth } from './GraphHealth';
import { EraseMemory } from './EraseMemory';
import { OvernightLearning } from './OvernightLearning';
import { DB_REPAIR_CONFIRM, runDbRepair } from './dbRepair';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import { useMemoryStatus } from '@/hooks/useMemoryStatus';

/** Fixed id so the progress subscription can be registered before the
    download starts (unlike a user-initiated one, which gets a fresh id). */
const EMBEDDING_DOWNLOAD_ID = 'adaptive-pathway-embedding-model';

const EMBEDDING_STATUS_LABEL: Record<EmbeddingModelStatus, string> = {
  unknown: '',
  present: 'Model ready',
  downloading: 'Downloading model…',
  missing: 'Model not downloaded',
};

/** Settings for Adaptive Pathway (behavioral memory): learns enduring habits,
    epistemic preferences, and stances from conversation while keeping every
    assumption visible and correctable. */
export function AdaptivePathway() {
  const memory = useMemoryStatus();
  const modelMissing = memory !== null && !memory.model_installed;
  const [enabled, setEnabled] = useState(false);
  const [embeddingStatus, setEmbeddingStatus] = useState<EmbeddingModelStatus>('unknown');
  const [embeddingModel, setEmbeddingModel] = useState('');
  const [installBusy, setInstallBusy] = useState(false);
  const [installError, setInstallError] = useState('');
  const [installProgress, setInstallProgress] = useState<DownloadProgress | null>(null);
  // HuggingFace token for the gated EmbeddingGemma repo. In-memory only for
  // this render; never persisted, never sent anywhere but the download request.
  const [token, setToken] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [mcpStatus, setMcpStatus] = useState<AdaptivePathwayMcpStatus | null>(null);
  const [mcpStatusError, setMcpStatusError] = useState('');
  const [refreshKey, setRefreshKey] = useState(0);
  const [showMaintenance, setShowMaintenance] = useState(false);
  const [repairing, setRepairing] = useState(false);
  const [repairMsg, setRepairMsg] = useState('');

  const bumpRefresh = () => setRefreshKey((k) => k + 1);

  const loadMcpStatus = () =>
    void ipc
      .getAdaptivePathwayMcpStatus()
      .then((s) => {
        setMcpStatus(s);
        setMcpStatusError('');
      })
      .catch((e) => setMcpStatusError(String(e)));

  const load = async () => {
    const cfg = await ipc.getConfig();
    setEnabled(cfg.adaptive_pathway_enabled);
    setEmbeddingModel(cfg.adaptive_pathway_embedding_model);
    loadMcpStatus();
  };

  useEffect(() => {
    void load();
    const unEmbedding = onAdaptivePathwayEmbeddingStatus((p) => setEmbeddingStatus(p.status));
    return () => void unEmbedding.then((fn) => fn());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const un = onModelProgress((p) => {
      if (p.download_id !== EMBEDDING_DOWNLOAD_ID) return;
      setInstallProgress(p);
    });
    return () => void un.then((fn) => fn());
  }, []);

  const installEmbeddingModel = async () => {
    setInstallBusy(true);
    setInstallError('');
    try {
      const model = defaultFor('embedding');
      if (!model) throw new Error('no embedding model is configured');
      await ipc.downloadModel(
        model.repo,
        model.file,
        undefined,
        EMBEDDING_DOWNLOAD_ID,
        model.gated ? token.trim() || undefined : undefined
      );
      setToken('');
    } catch (e) {
      setInstallError(String(e));
    } finally {
      setInstallBusy(false);
    }
  };

  const setEnabledCombined = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      setEnabled(next);
      await ipc.setAdaptivePathwayEnabled(next);
      loadMcpStatus();
      bumpRefresh();
    } catch (e) {
      setEnabled(!next);
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const repair = async () => {
    const ok = await confirmDialog({
      title: 'Check and repair the belief database?',
      message: DB_REPAIR_CONFIRM,
      confirmLabel: 'Check & repair',
    });
    if (!ok) return;
    setRepairing(true);
    setRepairMsg('');
    try {
      setRepairMsg(await runDbRepair(ipc.recoverPathwayDb));
      bumpRefresh();
    } catch (e) {
      setRepairMsg(String(e));
    } finally {
      setRepairing(false);
    }
  };

  return (
    <section className="settings-section memory-settings">
      <h1>Adaptive Pathway</h1>
      <p className="muted">
        Learns enduring habits, source preferences, and viewpoints from your chats while letting you
        inspect or forget any belief.
      </p>
      {error && <div className="chat-error">{error}</div>}

      <div className="mem-card">
        <label className="check">
          <input
            type="checkbox"
            checked={enabled && !modelMissing}
            disabled={busy || modelMissing}
            onChange={(e) => void setEnabledCombined(e.target.checked)}
          />
          <span>Enable Adaptive Pathway</span>
        </label>

        {enabled && (
          <div className="mem-status-strip">
            {mcpStatusError ? (
              <span className="mem-pill mem-pill-danger">Tools check failed: {mcpStatusError}</span>
            ) : mcpStatus == null ? (
              <span className="mem-pill">Registering tools…</span>
            ) : mcpStatus.status === 'connected' ? (
              <span className="mem-pill mem-pill-ok">
                Tools ready ({mcpStatus.tool_count}: record, forget)
              </span>
            ) : (
              <span className="mem-pill mem-pill-warn">
                Tools {mcpStatus.status}
                {mcpStatus.error_message ? ` (${mcpStatus.error_message})` : ''}
              </span>
            )}

            {embeddingStatus !== 'unknown' && embeddingStatus !== 'missing' && (
              <span className="mem-pill mem-pill-ok">
                {EMBEDDING_STATUS_LABEL[embeddingStatus]}
              </span>
            )}
          </div>
        )}

        {!enabled && (
          <small className="muted">
            {modelMissing
              ? 'Download the shared embedding model below to enable.'
              : 'Takes effect immediately when enabled.'}
          </small>
        )}
      </div>

      {(modelMissing || embeddingStatus === 'missing') && (
        <div className="mem-card">
          <div className="mem-card-head">
            <h2>Learning model required</h2>
            <button disabled={installBusy} onClick={() => void installEmbeddingModel()}>
              {installBusy ? 'Setting up…' : 'Set up learning model'}
            </button>
          </div>
          <p className="muted" style={{ margin: 0 }}>
            {isAndroid()
              ? `Adaptive Pathway uses ${embeddingModel || 'the embedding model'}. Chat works without it; memory activates once downloaded.`
              : `Both Adaptive Pathway and Memorabilia share ${embeddingModel || 'the embedding model'}. Chat works without it; memory activates once downloaded.`}
          </p>
          {defaultFor('embedding')?.gated && (
            <div className="gated-token">
              <label className="muted" htmlFor="hf-token-embedding">
                Gemma-licensed: accept the license on the{' '}
                <a
                  href={`https://huggingface.co/${defaultFor('embedding')?.repo ?? ''}`}
                  onClick={(e) => {
                    e.preventDefault();
                    const repo = defaultFor('embedding')?.repo ?? '';
                    if (repo) void ipc.openUrl(`https://huggingface.co/${repo}`).catch(() => {});
                  }}
                >
                  model page
                </a>{' '}
                and paste a read-only HuggingFace token (used only for this download).
              </label>
              <input
                id="hf-token-embedding"
                type="password"
                autoComplete="off"
                placeholder="hf_…"
                value={token}
                disabled={installBusy}
                onChange={(e) => setToken(e.target.value)}
              />
            </div>
          )}
          {installProgress && (
            <div className="pull-row">
              <div className="pull-head">
                <span>{installProgress.model}</span>
                <span className="muted">
                  {installProgress.error
                    ? `error: ${installProgress.error}`
                    : installProgress.done
                      ? 'done'
                      : 'downloading'}
                </span>
              </div>
              <div className="progress">
                <div
                  className="progress-bar"
                  style={{
                    width: installProgress.done
                      ? '100%'
                      : installProgress.total
                        ? `${Math.round((installProgress.received / installProgress.total) * 100)}%`
                        : '30%',
                  }}
                />
              </div>
            </div>
          )}
          {installError && (
            <p className="muted">Couldn&apos;t set up automatically: {installError}</p>
          )}
        </div>
      )}

      {enabled && !modelMissing && (
        <>
          <GraphHealth refreshKey={refreshKey} />
          <BeliefBrowser refreshKey={refreshKey} onMutated={bumpRefresh} />

          <div className="mem-card">
            <button
              type="button"
              className="disclosure-toggle"
              aria-expanded={showMaintenance}
              onClick={() => setShowMaintenance((v) => !v)}
            >
              {showMaintenance ? '▾' : '▸'} Maintenance &amp; reset
            </button>
            {showMaintenance && (
              <div className="provider-advanced-body">
                <div className="mem-danger-row" style={{ borderTop: 'none', paddingTop: 0 }}>
                  <div>
                    <div>Repair database</div>
                    <small className="muted">
                      Checks SQLite integrity and rebuilds from readable rows if corrupt.
                    </small>
                    {repairMsg && (
                      <div>
                        <small className="muted">{repairMsg}</small>
                      </div>
                    )}
                  </div>
                  <button onClick={() => void repair()} disabled={repairing}>
                    {repairing ? 'Checking…' : 'Check & repair'}
                  </button>
                </div>
                <EraseMemory
                  what="beliefs"
                  phrase="erase beliefs"
                  erase={ipc.eraseAllBeliefs}
                  onErased={bumpRefresh}
                />
              </div>
            )}
          </div>
        </>
      )}

      {!modelMissing && <OvernightLearning />}
    </section>
  );
}
