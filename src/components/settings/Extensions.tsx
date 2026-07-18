import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { ExtensionDefault } from '@/lib/types';
import { Modal } from '@/components/shared/Modal';

const extLabel = (e: ExtensionDefault): string => e.display_name ?? e.id;

/** `adaptive-pathway` is managed solely by the single enable checkbox in
    Settings → Advanced → Adaptive Pathway — showing it here too would be a
    confusing second control over the same thing. `replacement-mcp` gets its
    own dedicated card below (with the "replace built-ins?" offer), not the
    generic checkbox grid. */
const HIDDEN_EXTENSION_IDS = new Set(['adaptive-pathway', 'replacement-mcp']);

/** Default extensions for every new chat (Round-7 Feature 4) — reads/writes
    goose's own config.yaml directly (see src-tauri/src/goose_config.rs), the
    same file Goose Desktop's own extension settings edit. This is a genuine
    defaults editor, not a view of the currently active session: toggling
    something here changes what the *next* new session starts with, not the
    session already open (matching how a provider/temperature change needs a
    goosed restart to apply). Shows every extension goose knows about,
    including ones currently off — the old session-scoped ACP
    `extensions/list` could only ever show what was already attached to one
    live session, with no visibility into installed-but-inactive extensions. */
export function Extensions() {
  const [exts, setExts] = useState<ExtensionDefault[]>([]);
  const [error, setError] = useState('');
  const [form, setForm] = useState({ name: '', command: '', args: '', env: '' });
  const [adding, setAdding] = useState(false);

  const load = async () => {
    try {
      const all = await ipc.listDefaultExtensions();
      setExts(all.filter((e) => !HIDDEN_EXTENSION_IDS.has(e.id)));
    } catch (e) {
      setError(String(e));
    }
  };
  useEffect(() => void load(), []);

  const toggle = async (e: ExtensionDefault, enabled: boolean) => {
    try {
      await ipc.setDefaultExtensionEnabled(e.id, enabled);
      await load();
    } catch (err) {
      setError(String(err));
    }
  };

  const submitCustom = async () => {
    if (!form.name.trim() || !form.command.trim()) return;
    setAdding(true);
    try {
      await ipc.addExtension(
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
      <p className="muted">
        Enabled here means every new chat starts with it — this doesn&apos;t change a conversation
        already open.
      </p>
      {error && <div className="chat-error">{error}</div>}
      <ReplacementMcpCard onBuiltinsChanged={load} />

      <div className="ext-grid">
        {exts.map((e) => (
          <label className="ext-card" key={e.id}>
            <div className="ext-card-head">
              <span className="ext-card-name">{extLabel(e)}</span>
              <input
                type="checkbox"
                checked={e.enabled}
                onChange={(ev) => void toggle(e, ev.target.checked)}
              />
            </div>
            {e.description && <span className="muted ext-card-desc">{e.description}</span>}
          </label>
        ))}
        {exts.length === 0 && !error && <p className="muted">No extensions found.</p>}
      </div>

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
    </section>
  );
}

/** Dedicated card for the `replacement-mcp` internal plugin (see
    `plugins/replacement-mcp/`) — kept out of the generic checkbox grid above
    because turning it on offers to also disable Goose's built-in
    `developer`/`computercontroller` extensions, which it's designed to
    replace for local/small models. That offer is always a separate, explicit
    choice (CLAUDE.md B4) — accepting it never happens automatically. */
function ReplacementMcpCard({ onBuiltinsChanged }: { onBuiltinsChanged: () => void }) {
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [showReplaceOffer, setShowReplaceOffer] = useState(false);

  useEffect(() => {
    void ipc
      .getReplacementMcpEnabled()
      .then(setEnabled)
      .catch((e) => setError(String(e)));
  }, []);

  const toggle = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      await ipc.setReplacementMcpEnabled(next);
      setEnabled(next);
      if (next) setShowReplaceOffer(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const acceptReplaceOffer = async () => {
    try {
      await ipc.disableBuiltinDevExtensions();
      onBuiltinsChanged();
    } catch (e) {
      setError(String(e));
    } finally {
      setShowReplaceOffer(false);
    }
  };

  return (
    <>
      <label className="ext-card" style={{ marginBottom: 16 }}>
        <div className="ext-card-head">
          <span className="ext-card-name">Lean tools (replacement-mcp)</span>
          <input
            type="checkbox"
            checked={enabled}
            disabled={busy}
            onChange={(ev) => void toggle(ev.target.checked)}
          />
        </div>
        <span className="muted ext-card-desc">
          Context-optimized shell/file/web/document tools, designed to replace Goose&apos;s built-in
          Developer + Computer Controller extensions for local, small models. A goosed restart (or
          new session) is needed for the change to take effect.
        </span>
      </label>
      {error && <div className="chat-error">{error}</div>}

      {showReplaceOffer && (
        <Modal title="Replace the built-in extensions?">
          <p>
            Lean tools duplicates what Goose&apos;s built-in <strong>Developer</strong> and{' '}
            <strong>Computer Controller</strong> extensions do, just with smaller, more
            context-efficient tool descriptions. Disable those two now so the model isn&apos;t
            choosing between duplicate tools?
          </p>
          <div className="row">
            <button className="primary" onClick={() => void acceptReplaceOffer()}>
              Disable Developer + Computer Controller
            </button>
            <button onClick={() => setShowReplaceOffer(false)}>Keep both</button>
          </div>
        </Modal>
      )}
    </>
  );
}
