import { useEffect, useState } from 'react';
import { ipc, onEngineRestartState, onHotkeyFailed } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import { useRouteStore } from '@/stores/routeStore';
import { Banner } from '@/components/shared/Banner';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import type { EngineRestartState, RestartBlocker } from '@/lib/types';
import type { V1Data, V1ImportSummary } from '@/lib/ipc';
import { useSessionStore } from '@/stores/sessionStore';

/** What an import brought across, in a sentence. Pure. */
export function importSummaryText(s: V1ImportSummary): string {
  const parts = [
    `${s.sessions} chat${s.sessions === 1 ? '' : 's'}`,
    `${s.providers} provider${s.providers === 1 ? '' : 's'}`,
  ];
  if (s.mcp_servers) parts.push(`${s.mcp_servers} tool server${s.mcp_servers === 1 ? '' : 's'}`);
  let text = `Imported ${listNames(parts)} from your earlier Kitty.`;
  if (s.pathway === 'imported') text += ' What it had learned about you came too.';
  if (s.undecryptable_secrets > 0) {
    text += ` ${s.undecryptable_secrets} saved key${s.undecryptable_secrets === 1 ? '' : 's'} couldn't be read — enter ${s.undecryptable_secrets === 1 ? 'it' : 'them'} again in Settings → Providers.`;
  }
  return text;
}

/** "A and B" / "A, B and C". Pure. */
export function listNames(names: string[]): string {
  if (names.length <= 1) return names[0] ?? '';
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`;
}

/** What the restart banner says, or null when there is nothing to say. Pure. */
export function restartMessage(state: EngineRestartState, android: boolean): string | null {
  if (!state.reload_required && !state.restart_pending) return null;
  if (android) return 'A settings change takes effect the next time Kitty starts.';
  const others = unique(state.blocked_by);
  if (others.length > 0) {
    const verb = others.length === 1 ? 'is' : 'are';
    return `A settings change needs Kitty’s engine to restart, but ${listNames(others)} ${verb} using it. It restarts on its own once they’re done.`;
  }
  if (state.restart_pending) {
    return 'Kitty’s engine restarts after the current reply to apply a settings change.';
  }
  return 'A settings change takes effect when Kitty’s engine restarts.';
}

function unique(blockers: RestartBlocker[]): string[] {
  return [...new Set(blockers.map((b) => b.display_name))];
}

/** Hub-wide notices that are not about one chat: a settings change waiting
    on an engine restart (#2–4, #18), and a hotkey that could not be
    registered (#58). */
export function HubBanners() {
  const android = isAndroid();
  const goto = useRouteStore((s) => s.goto);
  const [restart, setRestart] = useState<EngineRestartState | null>(null);
  const [hotkeyErrors, setHotkeyErrors] = useState<string[]>([]);
  const [restarting, setRestarting] = useState(false);
  const [restartError, setRestartError] = useState<string | null>(null);
  const [hotkeyDismissed, setHotkeyDismissed] = useState(false);
  // Data from Kitty 0.8 or earlier, waiting to be brought across (#11).
  const [v1, setV1] = useState<V1Data | null>(null);
  const [importing, setImporting] = useState(false);
  const [importResult, setImportResult] = useState<string | null>(null);
  useEffect(() => {
    void ipc
      .detectV1Data()
      .then(setV1)
      .catch(() => {});
  }, []);
  const importV1 = async () => {
    setImporting(true);
    try {
      const summary = await ipc.importV1Data();
      setImportResult(importSummaryText(summary));
      setV1(null);
      void useSessionStore.getState().refresh();
    } catch (e) {
      setImportResult(String(e));
    } finally {
      setImporting(false);
    }
  };

  useEffect(() => {
    void ipc
      .getEngineRestartState()
      .then(setRestart)
      .catch(() => {});
    const unRestart = onEngineRestartState(setRestart);
    if (android) return () => void unRestart.then((f) => f());
    void ipc
      .getHotkeyFailures()
      .then(setHotkeyErrors)
      .catch(() => {});
    const unHotkey = onHotkeyFailed((e) => {
      setHotkeyErrors(e.errors);
      setHotkeyDismissed(false);
    });
    return () => {
      void unRestart.then((f) => f());
      void unHotkey.then((f) => f());
    };
  }, [android]);

  const restartNow = async (force: boolean) => {
    if (force) {
      const ok = await confirmDialog({
        title: 'Restart the engine anyway?',
        message: `${listNames(unique(restart?.blocked_by ?? []))} will lose the engine for a few seconds, and anything they are in the middle of stops.`,
        confirmLabel: 'Restart anyway',
        danger: true,
      });
      if (!ok) return;
    }
    setRestarting(true);
    setRestartError(null);
    try {
      const outcome = await ipc.restartBackend(force);
      if (!outcome.restarted && outcome.blocked_by.length > 0) {
        setRestart((s) => (s ? { ...s, blocked_by: outcome.blocked_by } : s));
      }
    } catch (e) {
      setRestartError(String(e));
    } finally {
      setRestarting(false);
    }
  };

  const message = restart ? restartMessage(restart, android) : null;
  const blocked = (restart?.blocked_by.length ?? 0) > 0;

  return (
    <>
      {v1 && (
        <Banner
          tone="warn"
          actions={
            <>
              <button className="link" disabled={importing} onClick={() => void importV1()}>
                {importing ? 'Importing…' : 'Import'}
              </button>
              <button
                className="link"
                disabled={importing}
                onClick={() => {
                  setV1(null);
                  void ipc.dismissV1Import().catch(() => {});
                }}
              >
                Don&apos;t import
              </button>
            </>
          }
        >
          Found chats and settings from an earlier version of Kitty (
          {Math.max(1, Math.round(v1.db_bytes / 1e6))} MB). Bring them into this one?
        </Banner>
      )}
      {importResult && (
        <Banner
          tone="ok"
          actions={
            <button className="link" onClick={() => setImportResult(null)}>
              Dismiss
            </button>
          }
        >
          {importResult}
        </Banner>
      )}
      {message && (
        <Banner
          tone="warn"
          actions={
            android ? undefined : blocked ? (
              <button className="link" disabled={restarting} onClick={() => void restartNow(true)}>
                Restart anyway
              </button>
            ) : restart?.restart_pending ? undefined : (
              <button className="link" disabled={restarting} onClick={() => void restartNow(false)}>
                {restarting ? 'Restarting…' : 'Restart now'}
              </button>
            )
          }
        >
          {message}
          {restartError && <span className="error"> Restart failed: {restartError}</span>}
        </Banner>
      )}
      {!android && hotkeyErrors.length > 0 && !hotkeyDismissed && (
        <Banner
          tone="warn"
          actions={
            <>
              <button
                className="link"
                onClick={() => goto('settings', { section: 'general', highlight: 'hotkeys' })}
              >
                Choose another hotkey
              </button>
              <button className="link" onClick={() => setHotkeyDismissed(true)}>
                Dismiss
              </button>
            </>
          }
        >
          A hotkey couldn’t be registered — another app may be using it. {hotkeyErrors.join(' ')}
        </Banner>
      )}
    </>
  );
}
