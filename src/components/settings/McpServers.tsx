import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { McpServer } from '@/lib/types';

/** `adaptive-pathway` is managed solely by the single enable checkbox in
    Settings → Advanced → Adaptive Pathway — showing it here too would be a
    confusing second control over the same thing. `replacement-mcp`,
    `wasm-math-mcp`, and `brave-mcp-search` each get their own dedicated card
    below, not the generic list. */
const HIDDEN_SERVER_NAMES = new Set([
  'adaptive-pathway',
  'replacement-mcp',
  'wasm-math-mcp',
  'brave-mcp-search',
]);

type Transport = 'stdio' | 'sse' | 'streamable_http';

const emptyForm = {
  name: '',
  transport: 'stdio' as Transport,
  command: '',
  args: '',
  url: '',
  env: '',
  apiKey: '',
};

/** `headers` only ever carries this one convenience shape from the UI — a
    bearer token typed into "API key" — even though the backend field itself
    is a generic header map (a power-user editing the raw MCP server config
    elsewhere could set something else there; this form just doesn't need
    to expose that generality). Empty input means "no auth header at all",
    not an empty-string token. */
function headersFromApiKey(apiKey: string): Record<string, string> | undefined {
  const trimmed = apiKey.trim();
  return trimmed ? { Authorization: `Bearer ${trimmed}` } : undefined;
}

/** Reverse of `headersFromApiKey`, for populating the form when editing an
    existing server — only recognizes its own convention (a bare `Bearer `
    Authorization header); anything else (a custom header set outside this
    form) is left out of the field rather than guessed at. */
function apiKeyFromHeaders(headers: Record<string, string>): string {
  const auth = headers.Authorization ?? headers.authorization;
  return auth?.startsWith('Bearer ') ? auth.slice('Bearer '.length) : '';
}

/** Quote-aware whitespace tokenizer for the Args field — a plain `split(/\s+/)`
    tears a single argument containing a space (e.g. any Windows path under
    "...\Documents\Claude Code\...") into two separate array elements, which
    Node/whatever interpreter then sees as two positional args instead of one
    path, and fails to start with no useful error (confirmed real report:
    `node "...\Claude" "Code\...\index.js"` — silently exits, surfaces to the
    user as an opaque "No response from MCP server"). A double-quoted span is
    kept as one token with the quotes stripped; unquoted text still splits on
    whitespace exactly as before, so existing single-word args are unaffected.
    No backslash-escape support for embedded quotes — Windows paths never
    contain a `"`, so it isn't needed here. */
export function parseArgs(s: string): string[] {
  const args: string[] = [];
  const re = /"([^"]*)"|(\S+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(s)) !== null) {
    args.push(m[1] !== undefined ? m[1] : m[2]);
  }
  return args;
}

/** Inverse of `parseArgs`, for round-tripping an existing server's `args`
    array back into the editable text field — an arg containing whitespace
    must be re-quoted, or editing-and-resaving an already-correct server
    would silently reintroduce the exact splitting bug `parseArgs` fixes. */
export function formatArgs(args: string[]): string {
  return args.map((a) => (/\s/.test(a) ? `"${a}"` : a)).join(' ');
}

function parseEnv(s: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const pair of s.split(',')) {
    const [k, ...rest] = pair.split('=');
    const key = k?.trim();
    if (key) out[key] = rest.join('=').trim();
  }
  return out;
}

function formatEnv(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => `${k}=${v}`)
    .join(', ');
}

const statusLabel = (s: McpServer): string => {
  if (s.status === 'connected') return 'Connected';
  if (s.status === 'error') return s.error_message ? `Error: ${s.error_message}` : 'Error';
  return 'Disconnected';
};

/** MCP servers — the BigTiny-backed replacement for the old goosed-path
    "Extensions" settings. Servers are daemon-global and take effect live: no
    restart to add, edit, delete, or toggle. */
export function McpServers() {
  const [servers, setServers] = useState<McpServer[]>([]);
  const [error, setError] = useState('');
  const [form, setForm] = useState(emptyForm);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async () => {
    try {
      const all = await ipc.listMcpServers();
      setServers(all.filter((s) => !HIDDEN_SERVER_NAMES.has(s.name)));
    } catch (e) {
      setError(String(e));
    }
  };
  useEffect(() => void load(), []);

  const resetForm = () => {
    setForm(emptyForm);
    setEditingId(null);
  };

  const startEdit = (s: McpServer) => {
    setEditingId(s.id);
    setForm({
      name: s.name,
      transport: s.transport,
      command: s.command ?? '',
      args: formatArgs(s.args),
      url: s.url ?? '',
      env: formatEnv(s.env),
      apiKey: apiKeyFromHeaders(s.headers),
    });
  };

  const toggle = async (s: McpServer, enabled: boolean) => {
    setError('');
    try {
      await ipc.setMcpServerEnabled(s.id, enabled);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const retry = async (s: McpServer) => {
    setError('');
    try {
      await ipc.connectMcpServer(s.id);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (s: McpServer) => {
    if (!confirm(`Delete MCP server "${s.name}"?`)) return;
    setError('');
    try {
      await ipc.deleteMcpServer(s.id);
      if (editingId === s.id) resetForm();
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const isRemote = form.transport === 'sse' || form.transport === 'streamable_http';

  const submit = async () => {
    if (!form.name.trim()) return;
    if (form.transport === 'stdio' && !form.command.trim()) return;
    if (isRemote && !form.url.trim()) return;
    setBusy(true);
    setError('');
    try {
      if (editingId) {
        await ipc.updateMcpServer(editingId, {
          name: form.name.trim(),
          transport: form.transport,
          command: form.transport === 'stdio' ? form.command.trim() : null,
          args: form.transport === 'stdio' ? parseArgs(form.args) : [],
          url: isRemote ? form.url.trim() : null,
          env: parseEnv(form.env),
          headers: isRemote ? (headersFromApiKey(form.apiKey) ?? {}) : {},
        });
      } else {
        await ipc.addMcpServer({
          name: form.name.trim(),
          transport: form.transport,
          command: form.transport === 'stdio' ? form.command.trim() : null,
          args: form.transport === 'stdio' ? parseArgs(form.args) : [],
          url: isRemote ? form.url.trim() : null,
          env: parseEnv(form.env),
          headers: isRemote ? headersFromApiKey(form.apiKey) : undefined,
          enabled: true,
        });
      }
      resetForm();
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="settings-section">
      <h1>MCP Servers</h1>
      <p className="muted">
        Tools available to the agent. Changes here take effect immediately — no restart needed.
      </p>
      {error && <div className="chat-error">{error}</div>}
      <ReplacementMcpCard />
      <WasmMathMcpCard />
      <BraveMcpSearchCard />

      <div className="ext-grid">
        {servers.map((s) => (
          <div className="ext-card" key={s.id}>
            <div className="ext-card-head">
              <span className="ext-card-name">{s.name}</span>
              <input
                type="checkbox"
                checked={s.enabled}
                onChange={(ev) => void toggle(s, ev.target.checked)}
              />
            </div>
            <span className="muted ext-card-desc">
              {s.transport === 'stdio' ? s.command : s.url} — {statusLabel(s)}
            </span>
            <div className="row" style={{ marginTop: 8 }}>
              <button onClick={() => startEdit(s)}>Edit</button>
              {s.status !== 'connected' && s.enabled && (
                <button onClick={() => void retry(s)}>Retry connect</button>
              )}
              <button onClick={() => void remove(s)}>Delete</button>
            </div>
          </div>
        ))}
        {servers.length === 0 && !error && <p className="muted">No MCP servers configured.</p>}
      </div>

      <div className="field" style={{ marginTop: 16 }}>
        <span>{editingId ? 'Edit MCP server' : 'Add MCP server'}</span>
        <div className="row">
          <input
            placeholder="Name"
            value={form.name}
            onChange={(e) => setForm({ ...form, name: e.target.value })}
          />
          <select
            value={form.transport}
            onChange={(e) => setForm({ ...form, transport: e.target.value as Transport })}
          >
            <option value="stdio">stdio</option>
            <option value="sse">sse (legacy HTTP+SSE)</option>
            <option value="streamable_http">Streamable HTTP</option>
          </select>
        </div>
        {form.transport === 'stdio' ? (
          <>
            <input
              placeholder="Command"
              value={form.command}
              onChange={(e) => setForm({ ...form, command: e.target.value })}
            />
            <input
              placeholder='Args (space-separated; use "quotes" around a value with spaces)'
              value={form.args}
              onChange={(e) => setForm({ ...form, args: e.target.value })}
            />
            <input
              placeholder="Env (KEY=VALUE, comma-separated)"
              value={form.env}
              onChange={(e) => setForm({ ...form, env: e.target.value })}
            />
          </>
        ) : (
          <>
            <input
              placeholder="Server URL"
              value={form.url}
              onChange={(e) => setForm({ ...form, url: e.target.value })}
            />
            <input
              type="password"
              placeholder="API key (optional — sent as a Bearer token)"
              value={form.apiKey}
              onChange={(e) => setForm({ ...form, apiKey: e.target.value })}
            />
          </>
        )}
        <div className="row">
          <button
            className="primary"
            disabled={
              busy ||
              !form.name.trim() ||
              (form.transport === 'stdio' ? !form.command.trim() : !form.url.trim())
            }
            onClick={() => void submit()}
          >
            {busy ? 'Saving…' : editingId ? 'Save changes' : 'Add server'}
          </button>
          {editingId && <button onClick={resetForm}>Cancel</button>}
        </div>
      </div>
    </section>
  );
}

/** Dedicated card for the bundled `replacement-mcp` server (see
    `plugins/replacement-mcp/`) — kept out of the generic list above since
    it's a Kitty-managed builtin, not a user-added one. */
function ReplacementMcpCard() {
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

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
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
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
          Context-optimized shell/file/web/document tools for local, small models.
        </span>
      </label>
      {error && <div className="chat-error">{error}</div>}
    </>
  );
}

/** Dedicated card for the bundled `wasm-math-mcp` server (see
    `plugins/wasm-math-mcp/`) — sandboxed Python/NumPy execution, on by
    default. Same shape as `ReplacementMcpCard`: no credentials, a plain
    toggle. */
function WasmMathMcpCard() {
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    void ipc
      .getWasmMathMcpEnabled()
      .then(setEnabled)
      .catch((e) => setError(String(e)));
  }, []);

  const toggle = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      await ipc.setWasmMathMcpEnabled(next);
      setEnabled(next);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <label className="ext-card" style={{ marginBottom: 16 }}>
        <div className="ext-card-head">
          <span className="ext-card-name">Math (wasm-math-mcp)</span>
          <input
            type="checkbox"
            checked={enabled}
            disabled={busy}
            onChange={(ev) => void toggle(ev.target.checked)}
          />
        </div>
        <span className="muted ext-card-desc">
          Sandboxed Python execution for exact math, stats, and NumPy — no shell, no filesystem,
          no network access.
        </span>
      </label>
      {error && <div className="chat-error">{error}</div>}
    </>
  );
}

/** Dedicated card for the bundled `brave-mcp-search` server (see
    `plugins/brave-mcp-search/`) — off by default, requires a Brave Search API
    key. Unlike every other builtin card, "enabled" and "configured" are
    tracked separately: disabling always wipes the stored key server-side
    (`ipc.setBraveMcpSearchEnabled(false)`), so the checkbox alone can never
    turn it back on — re-enabling always re-opens the API key form. This is
    deliberate (see `brave_mcp_search_enabled`'s doc comment in Rust
    `config/mod.rs`), not a rough edge to smooth over. */
function BraveMcpSearchCard() {
  const [enabled, setEnabled] = useState(false);
  const [configured, setConfigured] = useState(false);
  const [apiKey, setApiKey] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  const load = () =>
    ipc
      .getBraveMcpSearchStatus()
      .then((s) => {
        setEnabled(s.enabled);
        setConfigured(s.configured);
      })
      .catch((e) => setError(String(e)));

  useEffect(() => void load(), []);

  const disable = async () => {
    setBusy(true);
    setError('');
    try {
      await ipc.setBraveMcpSearchEnabled(false);
      setApiKey('');
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const saveKey = async () => {
    if (!apiKey.trim()) return;
    setBusy(true);
    setError('');
    try {
      await ipc.setBraveMcpSearchApiKey(apiKey);
      setApiKey('');
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  // The server is only really on when both halves agree: the intent flag
  // (app config) and a key actually being present (Windows Credential
  // Manager). They live in different stores, so they *can* drift apart —
  // confirmed real bug: archiving/resetting config.json left `enabled:
  // false` next to a surviving credential, and the old `!configured` gate
  // on the key form meant that state rendered an unchecked checkbox with no
  // form and no way back on. The form is now gated on `!isOn`, so any
  // not-fully-on state (including either half of a drift) is recoverable by
  // just entering a key, which rewrites both halves in one step.
  const isOn = enabled && configured;

  return (
    <>
      <div className="ext-card" style={{ marginBottom: 16 }}>
        <div className="ext-card-head">
          <span className="ext-card-name">Web search (brave-mcp-search)</span>
          <input
            type="checkbox"
            checked={isOn}
            disabled={busy}
            onChange={(ev) => {
              if (!ev.target.checked) void disable();
              // Checking it does nothing by itself — the API key form below
              // (shown whenever !isOn) is what actually turns it on.
            }}
          />
        </div>
        <span className="muted ext-card-desc">
          Brave Search LLM Context API. Requires an API key — turning this off always clears the
          stored key, so turning it back on requires entering it again.
        </span>
        {!isOn && configured && (
          <span className="muted ext-card-desc">
            A saved key was found but the server is switched off. Enter a key to turn it back
            on — this replaces the saved one.
          </span>
        )}
        {!isOn && (
          <div className="row" style={{ marginTop: 8 }}>
            <input
              type="password"
              placeholder="Brave Search API key"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  void saveKey();
                }
              }}
            />
            <button
              className="primary"
              disabled={busy || !apiKey.trim()}
              onClick={() => void saveKey()}
            >
              {busy ? 'Saving…' : 'Enable'}
            </button>
          </div>
        )}
      </div>
      {error && <div className="chat-error">{error}</div>}
    </>
  );
}
