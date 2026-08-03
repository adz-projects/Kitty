import { useEffect, useRef, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { DepStatus, Detection } from '@/lib/types';
import { ErrorDetail } from '@/components/shared/ErrorDetail';

const OLLAMA_RELEASES_URL = 'https://github.com/ollama/ollama/releases/latest';

/** How long to keep polling after handing off to an external installer
    before giving up and telling the user to re-check manually (Ollama has
    no "installer finished" signal Kitty can observe directly). */
const INSTALL_POLL_TIMEOUT_MS = 120_000;
const INSTALL_POLL_INTERVAL_MS = 2_000;

export function DetectStep({ onBack, onNext }: { onBack: () => void; onNext: () => void }) {
  const [det, setDet] = useState<Detection | null>(null);
  const [detecting, setDetecting] = useState(false);

  const detect = async () => {
    setDetecting(true);
    try {
      setDet(await ipc.detectDependencies());
    } finally {
      setDetecting(false);
    }
  };

  useEffect(() => {
    void detect();
  }, []);

  return (
    <section className="wizard-panel">
      <h1>Set up local models</h1>
      <p className="muted">Kitty needs Ollama installed to run models locally. One click below.</p>
      {detecting && !det && <p className="muted">Checking what's already installed…</p>}
      {det && <DepRow dep={det.ollama} onChanged={detect} />}
      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        <button onClick={() => void detect()}>Re-check</button>
        <button className="primary" disabled={!det || !det.ollama.installed} onClick={onNext}>
          Next
        </button>
      </div>
    </section>
  );
}

function DepRow({ dep, onChanged }: { dep: DepStatus; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const [waitingForInstaller, setWaitingForInstaller] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancelled = useRef(false);

  useEffect(
    () => () => {
      cancelled.current = true;
    },
    []
  );

  const install = async () => {
    setBusy(true);
    setError(null);
    try {
      await ipc.installDependency('ollama');
      // Ollama's installer runs in its own window with no "done" signal
      // Kitty can observe directly — poll detection instead of re-checking
      // once immediately after the (near-instant) spawn call returns, which
      // used to read a successful launch as a failure.
      setWaitingForInstaller(true);
      const deadline = Date.now() + INSTALL_POLL_TIMEOUT_MS;
      let installed = false;
      while (Date.now() < deadline && !cancelled.current) {
        await new Promise((r) => setTimeout(r, INSTALL_POLL_INTERVAL_MS));
        const fresh = await ipc.detectDependencies();
        if (fresh.ollama.installed) {
          installed = true;
          break;
        }
      }
      setWaitingForInstaller(false);
      if (!installed && !cancelled.current) {
        setError(
          "Didn't detect a finished install after 2 minutes. Finish the installer window, then click Re-check."
        );
      }
      onChanged();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="dep-row" style={{ flexDirection: 'column', alignItems: 'stretch', gap: 8 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <div>
          <strong>Ollama</strong>{' '}
          {dep.installed ? (
            <span className="status-badge">✓ {dep.version ?? 'installed'}</span>
          ) : (
            <span className="status-badge">not found</span>
          )}
          {dep.is_outdated && (
            <>
              {' '}
              <span className="status-badge status-badge-warn">
                Update available: {dep.latest_version}
              </span>
            </>
          )}
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          {!dep.installed && (
            <button disabled={busy} onClick={() => void install()}>
              {waitingForInstaller
                ? 'Waiting for install to finish…'
                : busy
                  ? 'Installing…'
                  : 'Install'}
            </button>
          )}
          {dep.is_outdated && (
            <button onClick={() => void ipc.openPath(OLLAMA_RELEASES_URL)}>View release</button>
          )}
        </div>
      </div>
      {waitingForInstaller && (
        <p className="muted" style={{ margin: 0, fontSize: 12 }}>
          Finish the installer window that just opened, then this will pick it up automatically — no
          need to click Re-check.
        </p>
      )}
      {error && (
        <ErrorDetail
          summary="Couldn't install Ollama automatically. You can also grab it yourself."
          raw={error}
        />
      )}
      {error && (
        <button className="link" onClick={() => void ipc.openPath(OLLAMA_RELEASES_URL)}>
          Open the Ollama download page ↗
        </button>
      )}
    </div>
  );
}
