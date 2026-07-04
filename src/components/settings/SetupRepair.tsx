import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useStackStore } from '@/stores/stackStore';

/** Setup & Repair: restart stack components; the full first-run wizard is Phase 7. */
export function SetupRepair() {
  const status = useStackStore((s) => s.status);
  const init = useStackStore((s) => s.init);
  const [msg, setMsg] = useState('');

  useEffect(() => void init(), [init]);

  const act = async (label: string, fn: () => Promise<void>) => {
    setMsg(`${label}…`);
    try {
      await fn();
      setMsg(`${label} — done.`);
    } catch (e) {
      setMsg(String(e));
    }
  };

  return (
    <section className="settings-section">
      <h1>Setup &amp; Repair</h1>
      <p>
        Stack status: <strong>{status.replace(/_/g, ' ')}</strong>
      </p>
      <div className="row">
        <button onClick={() => void act('Restarting Goose', () => ipc.restartGoosed())}>
          Restart Goose
        </button>
        <button onClick={() => void act('Restarting Ollama', () => ipc.restartOllama())}>
          Restart Ollama
        </button>
      </div>
      {msg && <p className="muted">{msg}</p>}
      <p className="muted">
        The guided first-run wizard (install Ollama/Goose, pull a starter model) arrives in Phase 7.
      </p>
    </section>
  );
}
