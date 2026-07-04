import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';

interface ExtRow {
  name: string;
  enabled?: boolean;
  [k: string]: unknown;
}

/** Goose extensions for the active session (ACP unstable extension methods).
    Session-scoped; toggling reflects in that session and future ones. */
export function Extensions() {
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [exts, setExts] = useState<ExtRow[]>([]);
  const [error, setError] = useState('');

  const load = async () => {
    const active = await ipc.getActiveSession();
    const sid = active?.session_id || null;
    setSessionId(sid);
    if (!sid) return;
    try {
      const list = (await ipc.listExtensions(sid)) as ExtRow[];
      setExts(list);
    } catch (e) {
      setError(String(e));
    }
  };
  useEffect(() => void load(), []);

  const toggle = async (name: string, enabled: boolean) => {
    if (!sessionId) return;
    try {
      await ipc.setExtensionEnabled(sessionId, name, enabled);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-section">
      <h1>Extensions</h1>
      {!sessionId && (
        <p className="muted">Open a chat session first — extensions are listed per session.</p>
      )}
      {error && <div className="chat-error">{error}</div>}
      <div className="ext-list">
        {exts.map((e) => (
          <label className="check" key={e.name}>
            <input
              type="checkbox"
              checked={e.enabled !== false}
              onChange={(ev) => void toggle(e.name, ev.target.checked)}
            />
            <span>{e.name}</span>
          </label>
        ))}
        {sessionId && exts.length === 0 && !error && <p className="muted">No extensions listed.</p>}
      </div>
      <p className="muted">
        Adding custom stdio/HTTP extensions is planned; toggling built-ins works here.
      </p>
    </section>
  );
}
