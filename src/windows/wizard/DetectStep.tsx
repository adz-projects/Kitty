import { useEffect, useRef, useState } from 'react';
import { ipc, pickExecutable } from '@/lib/ipc';
import type { Config, DepStatus, Detection } from '@/lib/types';
import { ErrorDetail } from '@/components/shared/ErrorDetail';

const RELEASES_URL: Record<'ollama' | 'goose', string> = {
  ollama: 'https://github.com/ollama/ollama/releases/latest',
  goose: 'https://github.com/aaif-goose/goose/releases/latest',
};

/** How long to keep polling after handing off to an external installer
    before giving up and telling the user to re-check manually (Ollama has
    no "installer finished" signal Kitty can observe directly). */
const INSTALL_POLL_TIMEOUT_MS = 120_000;
const INSTALL_POLL_INTERVAL_MS = 2_000;

export function DetectStep({
  cfg,
  onBack,
  onNext,
}: {
  cfg: Config;
  onBack: () => void;
  onNext: () => void;
}) {
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
      <p className="muted">
        Kitty needs two things installed: Ollama (runs the models) and Goose (Kitty's agent engine).
        Both install with one click below.
      </p>
      {detecting && !det && <p className="muted">Checking what's already installed…</p>}
      {det && (
        <>
          <DepRow name="Ollama" dep={det.ollama} which="ollama" cfg={cfg} onChanged={detect} />
          <DepRow name="Goose" dep={det.goose} which="goose" cfg={cfg} onChanged={detect} />
        </>
      )}
      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        <button onClick={() => void detect()}>Re-check</button>
        <button
          className="primary"
          disabled={!det || !det.ollama.installed || !det.goose.installed}
          onClick={onNext}
        >
          Next
        </button>
      </div>
    </section>
  );
}

function DepRow({
  name,
  dep,
  which,
  cfg,
  onChanged,
}: {
  name: string;
  dep: DepStatus;
  which: 'ollama' | 'goose';
  cfg: Config;
  onChanged: () => void;
}) {
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
      await ipc.installDependency(which);
      if (which === 'ollama') {
        // Ollama's installer runs in its own window with no "done" signal
        // Kitty can observe directly — poll detection instead of re-checking
        // once immediately after the (near-instant) spawn call returns,
        // which used to read a successful launch as a failure.
        setWaitingForInstaller(true);
        const deadline = Date.now() + INSTALL_POLL_TIMEOUT_MS;
        while (Date.now() < deadline && !cancelled.current) {
          await new Promise((r) => setTimeout(r, INSTALL_POLL_INTERVAL_MS));
          const fresh = await ipc.detectDependencies();
          if (fresh.ollama.installed) break;
        }
        setWaitingForInstaller(false);
      }
      onChanged();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const pickExisting = async () => {
    const path = await pickExecutable();
    if (!path) return;
    await ipc.setConfig({ ...cfg, goose_binary_override: path });
    onChanged();
  };

  return (
    <div className="dep-row" style={{ flexDirection: 'column', alignItems: 'stretch', gap: 8 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <div>
          <strong>{name}</strong>{' '}
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
          {!dep.installed && which === 'goose' && (
            <button disabled={busy} className="link" onClick={() => void pickExisting()}>
              I already have it
            </button>
          )}
          {dep.is_outdated && (
            <button onClick={() => void ipc.openPath(RELEASES_URL[which])}>View release</button>
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
          summary={`Couldn't install ${name} automatically. You can also grab it yourself.`}
          raw={error}
        />
      )}
      {error && (
        <button className="link" onClick={() => void ipc.openPath(RELEASES_URL[which])}>
          Open the {name} download page ↗
        </button>
      )}
    </div>
  );
}
