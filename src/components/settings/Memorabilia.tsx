import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { MemorabiliaMcpStatus } from '@/lib/types';
import { MemorabiliaBrowser } from './MemorabiliaBrowser';
import { MemorabiliaHealth } from './MemorabiliaHealth';
import { EraseMemory } from './EraseMemory';
import { OvernightLearning } from './OvernightLearning';
import { DB_REPAIR_CONFIRM, runDbRepair } from './dbRepair';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import { useMemoryStatus } from '@/hooks/useMemoryStatus';
import { useRouteStore } from '@/stores/routeStore';

/** Settings for Memorabilia (factual memory): extracts durable claims from
    shared documents, attached files, and scraped pages, and lets you inspect,
    filter, schedule, or forget stored facts. */
export function Memorabilia() {
  const memory = useMemoryStatus();
  const modelMissing = memory !== null && !memory.model_installed;
  const goto = useRouteStore((s) => s.goto);
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [mcpStatus, setMcpStatus] = useState<MemorabiliaMcpStatus | null>(null);
  const [mcpStatusError, setMcpStatusError] = useState('');
  const [refreshKey, setRefreshKey] = useState(0);
  const [showMaintenance, setShowMaintenance] = useState(false);
  const [repairing, setRepairing] = useState(false);
  const [repairMsg, setRepairMsg] = useState('');

  const bumpRefresh = () => setRefreshKey((k) => k + 1);

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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const setEnabledCombined = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      setEnabled(next);
      await ipc.setMemorabiliaEnabled(next);
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
      title: 'Check and repair the fact database?',
      message: DB_REPAIR_CONFIRM,
      confirmLabel: 'Check & repair',
    });
    if (!ok) return;
    setRepairing(true);
    setRepairMsg('');
    try {
      setRepairMsg(await runDbRepair(ipc.recoverMemorabiliaDb));
      bumpRefresh();
    } catch (e) {
      setRepairMsg(String(e));
    } finally {
      setRepairing(false);
    }
  };

  return (
    <section className="settings-section memory-settings">
      <h1>Memorabilia</h1>
      <p className="muted">
        Extracts corroborated facts from pasted text, attached files, and scraped pages so relevant
        knowledge returns automatically when needed.
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
          <span>Enable Memorabilia</span>
        </label>

        {modelMissing ? (
          <small className="muted">
            Download the shared embedding model to enable —{' '}
            <button
              className="link"
              onClick={() => goto('settings', { section: 'adaptive_pathway' })}
            >
              set it up in Adaptive Pathway →
            </button>
          </small>
        ) : enabled ? (
          <div className="mem-status-strip">
            {mcpStatusError ? (
              <span className="mem-pill mem-pill-danger">Tools check failed: {mcpStatusError}</span>
            ) : mcpStatus == null ? (
              <span className="mem-pill">Registering tools…</span>
            ) : mcpStatus.status === 'connected' ? (
              <span className="mem-pill mem-pill-ok">
                Tools ready ({mcpStatus.tool_count}: search, read item)
              </span>
            ) : (
              <span className="mem-pill mem-pill-warn">
                Tools {mcpStatus.status}
                {mcpStatus.error_message ? ` (${mcpStatus.error_message})` : ''}
              </span>
            )}
            <span className="mem-pill">Shares Adaptive Pathway embedding model</span>
          </div>
        ) : (
          <small className="muted">Takes effect immediately when enabled.</small>
        )}
      </div>

      {!modelMissing && <OvernightLearning />}

      {enabled && !modelMissing && (
        <>
          <MemorabiliaHealth refreshKey={refreshKey} />
          <MemorabiliaBrowser refreshKey={refreshKey} onMutated={bumpRefresh} />

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
                  what="facts"
                  phrase="erase facts"
                  erase={ipc.eraseAllFacts}
                  onErased={bumpRefresh}
                />
              </div>
            )}
          </div>
        </>
      )}
    </section>
  );
}
