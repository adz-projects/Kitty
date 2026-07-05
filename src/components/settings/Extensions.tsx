import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';

/** An extension entry from `_goose/unstable/session/extensions/list`. Tagged by
    `type`: `builtin`/`platform` entries have a top-level `name`; `mcp` entries
    do NOT — their identity lives at `server.name` (Round-2 Batch-0 probe). */
interface ExtRow {
  type?: 'builtin' | 'platform' | 'mcp';
  name?: string;
  display_name?: string;
  description?: string;
  server?: { name?: string; command?: string; args?: string[]; env?: string[] };
  enabled?: boolean;
  [k: string]: unknown;
}

/** Identity to key/toggle by — mcp entries fall back to `server.name` (this is
    the fix for Round-3 item 15's blank-checkbox bug: mcp entries have no
    top-level `name`, so the old `e.name` render was always empty for them). */
const extKey = (e: ExtRow): string =>
  e.name ?? e.server?.name ?? e.display_name ?? '(unnamed extension)';
const extLabel = (e: ExtRow): string => e.display_name ?? extKey(e);

/** Goose extensions for the active session (ACP unstable extension methods).
    Session-scoped; toggling reflects in that session and future ones. */
export function Extensions() {
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [exts, setExts] = useState<ExtRow[]>([]);
  const [error, setError] = useState('');
  const [form, setForm] = useState({ name: '', command: '', args: '', env: '' });
  const [adding, setAdding] = useState(false);

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

  const toggle = async (e: ExtRow, enabled: boolean) => {
    if (!sessionId) return;
    try {
      await ipc.setExtensionEnabled(sessionId, extKey(e), enabled, e.type, e.server);
      await load();
    } catch (err) {
      setError(String(err));
    }
  };

  const submitCustom = async () => {
    if (!sessionId || !form.name.trim() || !form.command.trim()) return;
    setAdding(true);
    try {
      await ipc.addExtension(
        sessionId,
        form.name.trim(),
        form.command.trim(),
        form.args.split(/\s+/).filter(Boolean),
        form.env
          .split(',')
          .map((s) => s.trim())
          .filter(Boolean)
      );
      setForm({ name: '', command: '', args: '', env: '' });
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setAdding(false);
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
          <label className="check" key={extKey(e)}>
            <input
              type="checkbox"
              checked={e.enabled !== false}
              onChange={(ev) => void toggle(e, ev.target.checked)}
            />
            <span>
              {extLabel(e)}
              {e.description && (
                <div className="muted" style={{ fontSize: 11 }}>
                  {e.description}
                </div>
              )}
            </span>
          </label>
        ))}
        {sessionId && exts.length === 0 && !error && <p className="muted">No extensions listed.</p>}
      </div>

      {sessionId && (
        <div className="field" style={{ marginTop: 16 }}>
          <span>Add custom extension</span>
          <div className="row">
            <input
              placeholder="Name"
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
            />
            <input
              placeholder="Command"
              value={form.command}
              onChange={(e) => setForm({ ...form, command: e.target.value })}
            />
          </div>
          <input
            placeholder="Args (space-separated)"
            value={form.args}
            onChange={(e) => setForm({ ...form, args: e.target.value })}
          />
          <input
            placeholder="Env (KEY=VALUE, comma-separated)"
            value={form.env}
            onChange={(e) => setForm({ ...form, env: e.target.value })}
          />
          <div className="row">
            <button
              className="primary"
              disabled={adding || !form.name.trim() || !form.command.trim()}
              onClick={() => void submitCustom()}
            >
              {adding ? 'Adding…' : 'Add extension'}
            </button>
          </div>
        </div>
      )}
    </section>
  );
}
