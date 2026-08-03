import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { McpServer } from '@/lib/types';

/** `adaptive-pathway` is managed solely by the single enable checkbox in
    Settings → Advanced → Adaptive Pathway — showing it here too would be a
    confusing second control over the same thing. `wasm-math-mcp`,
    `kitty-tools`, and `kitty-docs-web` each get their own dedicated card
    below, not the generic list.

    `replacement-mcp`, `brave-mcp-search`, and `visualizations` are retired
    (their tools live inside `kitty-tools` now — see
    `bigtiny::mcp::remove_retired_builtins`) but stay listed here for one
    release as a guard, in case an older install's BigTiny DB still has a
    stale row under one of these names before this app version's startup
    cleanup runs. */
const HIDDEN_SERVER_NAMES = new Set([
  'adaptive-pathway',
  'replacement-mcp',
  'wasm-math-mcp',
  'brave-mcp-search',
  'visualizations',
  'kitty-tools',
  'kitty-docs-web',
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
  /** Whether the server being edited already has an auth header configured
      server-side — BigTiny redacts the real value in every response
      (`"***"`, encrypted at rest), so this is a presence flag, not
      something the real value could ever be read back into. Drives the
      "🔑 key stored" placeholder and the submit-time omit-if-untouched
      logic below, mirroring Providers.tsx's `has_secret` convention. */
  hasStoredKey: false,
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

/** Whether the server already has some auth header set — BigTiny redacts
    the real value to `"***"` in every response, so this can only ever be a
    presence check, never a real value to populate the edit field with
    (unlike the pre-encryption behavior this replaces, which read the real
    key back out of the response — that's no longer possible now that the
    server never echoes it). */
function hasApiKeyConfigured(headers: Record<string, string>): boolean {
  return Boolean(headers.Authorization ?? headers.authorization);
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
      apiKey: '',
      hasStoredKey: hasApiKeyConfigured(s.headers),
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
        // A blank API-key field while editing means "untouched" (the real
        // value is never read back from the server — it's redacted to
        // "***" in every response), not "clear it" — omit `headers` from
        // the patch entirely so BigTiny's own "field absent = don't touch"
        // contract leaves whatever's already stored alone. Still explicitly
        // clears headers when switching to a non-remote transport, since a
        // stdio server has no business carrying auth headers around.
        const headers = !isRemote
          ? {}
          : form.apiKey.trim()
            ? headersFromApiKey(form.apiKey)
            : undefined;
        await ipc.updateMcpServer(editingId, {
          name: form.name.trim(),
          transport: form.transport,
          command: form.transport === 'stdio' ? form.command.trim() : null,
          args: form.transport === 'stdio' ? parseArgs(form.args) : [],
          url: isRemote ? form.url.trim() : null,
          env: parseEnv(form.env),
          headers,
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
      <div className="ext-grid" style={{ marginBottom: 16 }}>
        <WasmMathMcpCard />
        <BraveMcpSearchCard />
        <VisualizationsCard />
        <KittyToolsCard />
        <KittyDocsWebCard />
      </div>

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
              placeholder={
                form.hasStoredKey
                  ? '🔑 key stored — leave blank to keep, or type to replace'
                  : 'API key (optional — sent as a Bearer token)'
              }
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

/** Dedicated card for the bundled `wasm-math-mcp` server (see
    `plugins/wasm-math-mcp/`) — sandboxed Python/NumPy execution, on by
    default. Same shape as `KittyToolsCard`'s toggle: no credentials, a plain
    checkbox. */
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
    <label className="ext-card">
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
        Sandboxed Python execution for exact math, stats, and NumPy — no shell, no filesystem, no
        network access.
      </span>
      {error && <div className="chat-error">{error}</div>}
    </label>
  );
}

/** Dedicated card for the visualization tools — accessible HTML tables, SVG
    diagrams, and charts, rendered inline in chat as their own always-visible
    card (see `VisualizationCard`). Hosted inside the combined `kitty-tools`
    server (see `KittyToolsCard` above); this toggle flips the
    `KITTY_VIZ_ENABLED` env var rather than spawning its own process. On by
    default, no credentials — same shape as `WasmMathMcpCard`. */
function VisualizationsCard() {
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    void ipc
      .getVisualizationsEnabled()
      .then(setEnabled)
      .catch((e) => setError(String(e)));
  }, []);

  const toggle = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      await ipc.setVisualizationsEnabled(next);
      setEnabled(next);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <label className="ext-card">
      <div className="ext-card-head">
        <span className="ext-card-name">Visuals (visualizations)</span>
        <input
          type="checkbox"
          checked={enabled}
          disabled={busy}
          onChange={(ev) => void toggle(ev.target.checked)}
        />
      </div>
      <span className="muted ext-card-desc">
        Accessible HTML tables, SVG diagrams, and charts, rendered inline in chat.
      </span>
      {error && <div className="chat-error">{error}</div>}
    </label>
  );
}

/** Dedicated card for the bundled `kitty-tools` server (see
    `plugins/kitty-tools/`) — the Rust consolidation of `replacement-mcp`'s
    18 shell/workspace/file/word/cache/scratchpad tools, plus the 3
    visualization tools (separately gated by `VisualizationsCard` below,
    which toggles an env var on this one process rather than spawning its
    own). Web search does NOT live here — see `BraveMcpSearchCard`/
    `KittyDocsWebCard` below, it moved to `kitty-docs-web`. On by default,
    no credentials required for this toggle itself — same shape as
    `WasmMathMcpCard`. */
function KittyToolsCard() {
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    void ipc
      .getKittyToolsEnabled()
      .then(setEnabled)
      .catch((e) => setError(String(e)));
  }, []);

  const toggle = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      await ipc.setKittyToolsEnabled(next);
      setEnabled(next);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <label className="ext-card">
      <div className="ext-card-head">
        <span className="ext-card-name">Lean tools (kitty-tools)</span>
        <input
          type="checkbox"
          checked={enabled}
          disabled={busy}
          onChange={(ev) => void toggle(ev.target.checked)}
        />
      </div>
      <span className="muted ext-card-desc">
        Context-optimized shell/file/Word tools for local, small models.
      </span>
      {error && <div className="chat-error">{error}</div>}
    </label>
  );
}

/** Dedicated card for the bundled `kitty-docs-web` server (see
    `plugins/kitty-docs-web/`) — PDF/Excel reading, web scraping, and the
    merged `lean_web_search`/`lean_web_search_read_chunk` web search tools
    (Brave preference controlled separately by `BraveMcpSearchCard` below;
    DuckDuckGo always works here with no key). The other half of the
    `replacement-mcp` split; on by default, no credentials — same shape as
    `WasmMathMcpCard`. */
function KittyDocsWebCard() {
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    void ipc
      .getKittyDocsWebEnabled()
      .then(setEnabled)
      .catch((e) => setError(String(e)));
  }, []);

  const toggle = async (next: boolean) => {
    setBusy(true);
    setError('');
    try {
      await ipc.setKittyDocsWebEnabled(next);
      setEnabled(next);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <label className="ext-card">
      <div className="ext-card-head">
        <span className="ext-card-name">PDF/Excel/Web (kitty-docs-web)</span>
        <input
          type="checkbox"
          checked={enabled}
          disabled={busy}
          onChange={(ev) => void toggle(ev.target.checked)}
        />
      </div>
      <span className="muted ext-card-desc">
        Read PDFs and Excel spreadsheets, scrape web pages, and search the web.
      </span>
      {error && <div className="chat-error">{error}</div>}
    </label>
  );
}

/** Dedicated card for Brave search preference — this toggle does not spawn
    its own process and does not gate whether `lean_web_search` exists at
    all (it always does, via `kitty-docs-web` — see `KittyDocsWebCard`
    above — since DuckDuckGo needs no key). It only controls whether
    `BRAVE_API_KEY` is present on that server's env, which makes
    `lean_web_search` prefer Brave (with automatic DuckDuckGo fallback) for
    small requests, and query both engines together for broader ones. Off
    by default, requires a Brave Search API key. Unlike every other builtin
    card, "enabled" and "configured" are tracked separately: disabling
    always wipes the stored key server-side
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
    <div className="ext-card">
      <div className="ext-card-head">
        <span className="ext-card-name">Web search: Brave (preferred engine)</span>
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
        stored key, so turning it back on requires entering it again. DuckDuckGo is always available
        as a fallback even without this.
      </span>
      {!isOn && configured && (
        <span className="muted ext-card-desc">
          A saved key was found but the server is switched off. Enter a key to turn it back on —
          this replaces the saved one.
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
      {error && <div className="chat-error">{error}</div>}
    </div>
  );
}
