import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { MemorabiliaMcpStatus } from '@/lib/types';
import { MemorabiliaBrowser } from './MemorabiliaBrowser';
import { MemorabiliaHealth } from './MemorabiliaHealth';
import { EraseMemory } from './EraseMemory';
import { useMemoryStatus } from '@/hooks/useMemoryStatus';
import { useRouteStore } from '@/stores/routeStore';

/** Settings for the memorabilia (declarative factual-memory) engine — the
    substantive-memory counterpart to Adaptive Pathway's behavioral memory.
    It distills durable factual claims from the documents a chat brings in
    (pasted text, attached files, scraped pages — `agent::memorabilia_harvest`),
    tracks their confidence from the supporting evidence, and surfaces a
    small, relevant slice back per turn. Shares Adaptive Pathway's learning
    model, and like it, runs only once that model is downloaded. */
export function Memorabilia() {
  const memory = useMemoryStatus();
  const modelMissing = memory !== null && !memory.model_installed;
  const goto = useRouteStore((s) => s.goto);
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [mcpStatus, setMcpStatus] = useState<MemorabiliaMcpStatus | null>(null);
  const [mcpStatusError, setMcpStatusError] = useState('');

  const loadMcpStatus = () =>
    void ipc
      .getMemorabiliaMcpStatus()
      .then((s) => {
        setMcpStatus(s);
        setMcpStatusError('');
      })
      .catch((e) => setMcpStatusError(String(e)));

  const load = async () => {
    const cfg = await ipc.getConfig();
    setEnabled(cfg.memorabilia_enabled);
    loadMcpStatus();
  };

  useEffect(() => {
    void load();
    // Mount-only by design (same reasoning as AdaptivePathway): `load` is a
    // fresh closure each render and captures nothing that changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Flips the config flag; the Rust command restarts BigTiny (which is what
      actually starts/stops the in-process engine — it's linked into that
      process, there's nothing separate to spawn) and self-heals the MCP
      registration, so the frontend just flips the flag and re-reads status
      once it settles. */
  const setEnabledCombined = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      setEnabled(next);
      await ipc.setMemorabiliaEnabled(next);
      loadMcpStatus();
    } catch (e) {
      // Revert the optimistic flip — the config flag never changed.
      setEnabled(!next);
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="settings-section">
      <h1>Memorabilia</h1>
      <p className="muted">
        Remembers durable facts from what you share in chats — pasted text, attached files and pages
        it reads — and brings the relevant ones back when they matter. Everything it holds is
        something you can see and correct below.
      </p>
      {error && <div className="chat-error">{error}</div>}

      <label className="check">
        <input
          type="checkbox"
          checked={enabled && !modelMissing}
          disabled={busy || modelMissing}
          onChange={(e) => void setEnabledCombined(e.target.checked)}
        />
        <span>Enable Memorabilia</span>
      </label>
      <small className="muted">
        {modelMissing ? (
          <>
            Download the memory model to enable it —{' '}
            <button
              className="link"
              onClick={() => goto('settings', { section: 'adaptive_pathway' })}
            >
              set it up in Adaptive Pathway
            </button>
            .
          </>
        ) : (
          'Takes effect straight away. Uses the same learning model as Adaptive Pathway.'
        )}
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
              {mcpStatus.tool_count === 1 ? '' : 's'} available to the model (search, read item).
            </>
          ) : (
            <>
              MCP server <strong>{mcpStatus.status}</strong> — tools not reaching the model.
              {mcpStatus.error_message ? ` ${mcpStatus.error_message}` : ''}
            </>
          )}
        </small>
      )}

      {enabled && !modelMissing && (
        <>
          <h2>What it remembers</h2>
          <MemorabiliaBrowser />
          <MemorabiliaHealth />
          <EraseMemory what="facts" phrase="erase facts" erase={ipc.eraseAllFacts} />
        </>
      )}
    </section>
  );
}
