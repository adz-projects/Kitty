import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { MemorabiliaMcpStatus } from '@/lib/types';
import { MemorabiliaBrowser } from './MemorabiliaBrowser';
import { MemorabiliaHealth } from './MemorabiliaHealth';

/** Settings for the memorabilia (declarative factual-memory) engine — the
    substantive-memory counterpart to Adaptive Pathway's behavioral memory.
    It distills durable factual claims from ordinary conversation, tracks
    their confidence from the supporting evidence, and surfaces a small,
    relevant slice back per turn (the model reads any item in full on demand
    via the `memorabilia_search` / `memorabilia_read_item` tools). Runs
    in-process inside the BigTiny daemon (see `plugins/memorabilia_rust`) and
    reuses the same shared learning model Adaptive Pathway uses — so there's
    no separate model to download here, just the one enable checkbox, which
    the daemon restart under `set_memorabilia_enabled` applies. */
export function Memorabilia() {
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
        Remembers durable facts from ordinary conversation — what&apos;s true about your work,
        your projects, and the things you tell it — and quietly brings the relevant ones back
        when they matter. Everything it holds is something you can see and correct below.
      </p>
      {error && <div className="chat-error">{error}</div>}

      <label className="check">
        <input
          type="checkbox"
          checked={enabled}
          disabled={busy}
          onChange={(e) => void setEnabledCombined(e.target.checked)}
        />
        <span>Enable Memorabilia</span>
      </label>
      <small className="muted">
        Runs in-process inside Kitty&apos;s local engine — restarting it applies this change. Uses
        the same learning model as Adaptive Pathway, so there&apos;s nothing extra to download.
      </small>

      {enabled && (
        <small className="muted" style={{ display: 'block', marginTop: 8 }}>
          {mcpStatusError ? (
            <>Couldn&apos;t check tool registration: {mcpStatusError}</>
          ) : mcpStatus == null ? (
            <>Tools not registered with BigTiny yet — will appear shortly.</>
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

      {enabled && (
        <>
          <h2>What it remembers</h2>
          <MemorabiliaBrowser />
          <MemorabiliaHealth />
        </>
      )}
    </section>
  );
}
