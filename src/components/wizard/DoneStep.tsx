import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useRouteStore } from '@/stores/routeStore';
import type { SetupValidation } from '@/lib/types';
import { isAndroid } from '@/lib/platform';

export function DoneStep({ onBack }: { onBack: () => void }) {
  const [validation, setValidation] = useState<SetupValidation | null>(null);
  const [checking, setChecking] = useState(true);
  const [finishing, setFinishing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const check = async () => {
    setChecking(true);
    setError(null);
    try {
      setValidation(await ipc.validateSetup());
    } catch (e) {
      setError(String(e));
    } finally {
      setChecking(false);
    }
  };

  useEffect(() => {
    void check();
  }, []);

  const finish = async () => {
    setFinishing(true);
    setError(null);
    try {
      await ipc.completeSetup();
      // The wizard is a route now, not a window Rust can hide, so finishing
      // has to navigate. Without this the hub sits on a completed wizard with
      // no way out but the browser-less equivalent of a back button.
      useRouteStore.getState().goto('chat');
    } catch (e) {
      setError(String(e));
    } finally {
      setFinishing(false);
    }
  };

  return (
    <section className="wizard-panel">
      <h1>You're all set</h1>
      <p className="muted">
        Press your hotkey any time to summon Kitty. You can re-run this from Settings → Setup &amp;
        Repair, or fine-tune everything from Settings once you're chatting.
      </p>

      <div className="wizard-summary">
        <div className="wizard-summary-row">
          <span className="muted">Chat</span>
          <span>Your own API key</span>
        </div>
        <div className="wizard-summary-row">
          <span className="muted">On this device</span>
          <span>
            Memory{!isAndroid() && ' (and summarization)'} via Kitty's own local model
            {!isAndroid() && 's'}
          </span>
        </div>
        {checking && (
          <p className="muted" style={{ margin: 0 }}>
            Checking everything's ready…
          </p>
        )}
        {validation && validation.ready && (
          <p className="muted" style={{ margin: 0 }}>
            Everything checks out. ✓
          </p>
        )}
        {validation && !validation.ready && (
          <>
            <p style={{ margin: 0, color: 'var(--danger)' }}>A couple of things to look at:</p>
            <ul className="wizard-issue-list">
              {validation.issues.map((issue) => (
                <li key={issue} className="muted">
                  {issue}
                </li>
              ))}
            </ul>
          </>
        )}
        {validation && (
          <p className="muted" style={{ margin: 0, fontSize: 12 }}>
            Adaptive Pathway (learns your preferences over time):{' '}
            {validation.adaptive_pathway_ok ? 'ready' : 'not running yet — see Settings → Advanced'}
          </p>
        )}
      </div>

      {error && <p className="error">{error}</p>}

      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        <button onClick={() => void check()} disabled={checking}>
          Re-check
        </button>
        <button className="primary" disabled={finishing} onClick={() => void finish()}>
          {finishing
            ? 'Starting…'
            : validation && !validation.ready
              ? 'Finish anyway'
              : 'Start chatting'}
        </button>
      </div>
    </section>
  );
}
