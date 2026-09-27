import { useEffect, useState } from 'react';
import { ipc, onAdaptivePathwayEmbeddingStatus, onModelProgress } from '@/lib/ipc';
import type { AdaptivePathwayMcpStatus, DownloadProgress, EmbeddingModelStatus } from '@/lib/types';
import { defaultFor } from '@/lib/curated_models';

/** Fixed id so the progress subscription can be registered before the
    download starts (unlike a user-initiated one, which gets a fresh id). */
const EMBEDDING_DOWNLOAD_ID = 'adaptive-pathway-embedding-model';
import { BeliefBrowser } from './BeliefBrowser';
import { GraphHealth } from './GraphHealth';
import { EraseMemory } from './EraseMemory';
import { useMemoryStatus } from '@/hooks/useMemoryStatus';

/** How far along the shared learning model's download is. Memory runs only
    once it is on disk (decision #1): without it the engines are off, not
    running on a weaker fallback. */
const EMBEDDING_STATUS_LABEL: Record<EmbeddingModelStatus, string> = {
  unknown: '',
  present: 'ready',
  downloading: 'downloading…',
  missing: 'not downloaded yet',
};

/** Settings for the pathway (behavioral-memory) engine — learns what the
    user cares about and how they like to be talked to, from ordinary
    conversation, and surfaces it back as a per-turn recall block. Runs
    inside the engine (`plugins/adaptive-pathway_rust`); the switch applies
    at once (`lifecycle::memory`), and only once the learning model is
    downloaded. */
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
    // Mount-only by design: `load` is a fresh closure every render, so
    // depending on it would re-run this on each one — re-reading config and
    // re-registering the listener. Nothing it captures changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const un = onModelProgress((p) => {
      if (p.download_id !== EMBEDDING_DOWNLOAD_ID) return;
      setInstallProgress(p);
    });
    return () => void un.then((fn) => fn());
  }, []);

  /** Settings' own "set up the learning model" action — the same download the
      wizard offers, reachable after setup too (the user skipped it, or
      deleted the file later). A fixed download id lets the subscription above
      be registered before the download starts. */
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
      setToken(''); // done with it — drop it from memory
    } catch (e) {
      setInstallError(String(e));
    } finally {
      setInstallBusy(false);
    }
  };

  /** Flips the config flag; the Rust command restarts BigTiny (which is
      what actually starts/stops the in-process engine — it's linked into
      that process, there's nothing separate to spawn) and self-heals the
      MCP registration, so the frontend just flips the flag and re-reads
      status once it settles. */
  const setEnabledCombined = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      setEnabled(next);
      await ipc.setAdaptivePathwayEnabled(next);
      loadMcpStatus();
    } catch (e) {
      // Revert the optimistic flip — the config flag never changed, so the
      // checkbox must not claim otherwise (same pattern as
      // AdaptivePathwayToggle's paused toggle).
      setEnabled(!next);
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="settings-section">
      <h1>Adaptive Pathway</h1>
      <p className="muted">
        Learns what you care about and how you like to be talked to from ordinary conversation, and
        quietly adapts — never as a fact stated about you, always something you can correct.
      </p>
      {error && <div className="chat-error">{error}</div>}

      <label className="check">
        <input
          type="checkbox"
          checked={enabled && !modelMissing}
          disabled={busy || modelMissing}
          onChange={(e) => void setEnabledCombined(e.target.checked)}
        />
        <span>Enable Adaptive Pathway</span>
      </label>
      <small className="muted">
        {modelMissing
          ? 'Download the memory model below to enable it.'
          : 'Takes effect straight away.'}
      </small>

      {enabled && (
        <small className="muted" style={{ display: 'block', marginTop: 8 }}>
          {mcpStatusError ? (
            <>Couldn&apos;t check tool registration: {mcpStatusError}</>
          ) : mcpStatus == null ? (
            <>Tools not registered yet — they&apos;ll appear shortly.</>
          ) : mcpStatus.status === 'connected' ? (
            <>
              Connected: <strong>{mcpStatus.tool_count}</strong> tool
              {mcpStatus.tool_count === 1 ? '' : 's'} available to the model (record, forget).
            </>
          ) : (
            <>
              MCP server <strong>{mcpStatus.status}</strong> — tools not reaching the model.
              {mcpStatus.error_message ? ` ${mcpStatus.error_message}` : ''}
            </>
          )}
        </small>
      )}

      {(modelMissing || embeddingStatus === 'missing') && (
        <div className="dep-row" style={{ flexDirection: 'column', alignItems: 'stretch', gap: 8 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <span className="muted">
              Memory needs the learning model ({embeddingModel}), which isn&apos;t downloaded yet.
              Chat works fine without it; memory stays off until it&apos;s here.
            </span>
            <button disabled={installBusy} onClick={() => void installEmbeddingModel()}>
              {installBusy ? 'Setting up…' : 'Set up learning model'}
            </button>
          </div>
          {defaultFor('embedding')?.gated && (
            <div className="gated-token">
              <label className="muted" htmlFor="hf-token-embedding">
                Gemma-licensed: accept the license on the{' '}
                <a
                  href={`https://huggingface.co/${defaultFor('embedding')?.repo ?? ''}`}
                  target="_blank"
                  rel="noreferrer"
                >
                  model page
                </a>{' '}
                and paste a HuggingFace access token (read scope). Used only for this download,
                never stored.
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
      {enabled && embeddingStatus !== 'unknown' && embeddingStatus !== 'missing' && (
        <small className="muted" style={{ display: 'block' }}>
          Learning model {EMBEDDING_STATUS_LABEL[embeddingStatus]}.
        </small>
      )}

      {enabled && !modelMissing && (
        <>
          <h2>What it remembers</h2>
          <BeliefBrowser />
          <GraphHealth />
          <EraseMemory what="beliefs" phrase="erase beliefs" erase={ipc.eraseAllBeliefs} />
        </>
      )}
    </section>
  );
}
