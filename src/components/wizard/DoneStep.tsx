import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useRouteStore } from '@/stores/routeStore';
import type { SetupValidation } from '@/lib/types';
import { isAndroid } from '@/lib/platform';
import { useMemoryStatus } from '@/hooks/useMemoryStatus';

export function DoneStep({ onBack }: { onBack: () => void }) {
  const [validation, setValidation] = useState<SetupValidation | null>(null);
  const [checking, setChecking] = useState(true);
  const [finishing, setFinishing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const memory = useMemoryStatus();
  const [summarizer, setSummarizer] = useState<'local' | 'provider' | null>(null);
  useEffect(() => {
    if (isAndroid()) return;
    void ipc
      .getSummarizerStatus()
      .then((s) => setSummarizer(s.effective))
      .catch(() => {});
  }, []);

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
        {isAndroid()
          ? 'You can run setup again from Settings → Advanced, and change anything else in Settings once you’re chatting.'
          : 'Press your hotkey any time to summon Kitty. You can run setup again from Settings → Advanced, and change anything else in Settings once you’re chatting.'}
      </p>

      <div className="wizard-summary">
        <div className="wizard-summary-row">
          <span className="muted">Chat</span>
          <span>Your own API key</span>
        </div>
        <div className="wizard-summary-row">
          <span className="muted">Memory</span>
          <span>
            {memory === null
              ? '…'
              : memory.pathway_active || memory.memorabilia_active
                ? 'On, on this device'
                : memory.model_installed
                  ? 'Off (turn it on in Settings)'
                  : 'Off until the memory model is downloaded'}
          </span>
        </div>
        {!isAndroid() && (
          <div className="wizard-summary-row">
            <span className="muted">Summarizing long chats</span>
            <span>
              {summarizer === null
                ? '…'
                : summarizer === 'local'
                  ? 'On this computer'
                  : 'By your chat provider'}
            </span>
          </div>
        )}
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
