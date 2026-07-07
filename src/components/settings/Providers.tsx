import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type {
  NetworkTier,
  OllamaModel,
  ProviderProfile,
  ProviderType,
  ProviderView,
} from '@/lib/types';
import { trustBadge } from '@/lib/provider_trust';
import {
  ANTHROPIC_CONTEXT_TABLE,
  CUSTOM_OPENAI_CONTEXT_TABLE,
  lookupContextLength,
} from '@/lib/context_length_table';

const DEFAULT_URL: Record<ProviderType, string> = {
  ollama: 'http://localhost:11434',
  openrouter: 'https://openrouter.ai/api/v1',
  anthropic: 'https://api.anthropic.com',
  openai: 'https://api.openai.com/v1',
  custom_openai: '',
};

// Context-length detents (item 28): not linearly spaced, so the slider indexes
// into this array rather than mapping its position directly to a value. When
// auto-detection (Round-6 Feature 1) finds a real number for the selected
// model, it's spliced in as an extra detent (see `detentsFor` below) rather
// than snapped to the nearest static stop, so the exact real max is always
// reachable and reads correctly on the badge.
const CTX_DETENTS = [4096, 8192, 16384, 32768, 65536, 131072, 262144];
const ctxLabel = (v: number) => (v % 1024 === 0 ? `${v / 1024}K` : String(v));
function nearestCtxIndex(detents: number[], v: number): number {
  let best = 0;
  let bd = Infinity;
  detents.forEach((d, i) => {
    const dist = Math.abs(d - v);
    if (dist < bd) {
      bd = dist;
      best = i;
    }
  });
  return best;
}

/** Static detents plus a live-detected value, if any and not already present. */
function detentsFor(suggested: number | null): number[] {
  if (suggested == null || CTX_DETENTS.includes(suggested)) return CTX_DETENTS;
  return [...CTX_DETENTS, suggested].sort((a, b) => a - b);
}

/** Best-effort context-window suggestion for the model currently selected on
    `profile`, per provider type (Round-6 Feature 1): Ollama/OpenRouter query
    live; Anthropic/custom_openai use a small hardcoded table. `null` when
    nothing is known — the field stays fully manual in that case. */
async function suggestContextLength(profile: ProviderProfile): Promise<number | null> {
  const model = profile.models[0];
  if (!model) return null;
  try {
    switch (profile.provider_type) {
      case 'ollama':
        return await ipc.ollamaShowContextLength(model);
      case 'openrouter':
        return await ipc.openrouterContextLength(model);
      case 'anthropic':
        return lookupContextLength(ANTHROPIC_CONTEXT_TABLE, model);
      case 'openai':
      case 'custom_openai':
        return lookupContextLength(CUSTOM_OPENAI_CONTEXT_TABLE, model);
      default:
        return null;
    }
  } catch {
    return null;
  }
}

/** Client-side mirror of providers::network_tier_for — only used to detect
    loopback (which is always "local"/trusted). */
function tierOf(url: string): NetworkTier {
  const host = (url.split('://').pop() ?? '').split('/')[0].split('@').pop() ?? '';
  const h = host.split(':')[0].toLowerCase();
  if (!h || h === 'localhost' || h === '127.0.0.1' || h === '::1') return 'local';
  if (h.endsWith('.ts.net')) return 'personal';
  const o = h.split('.').map(Number);
  if (o.length === 4 && o[0] === 100 && o[1] >= 64 && o[1] <= 127) return 'personal';
  return 'remote';
}

const isLocal = (url: string) => tierOf(url) === 'local';

const blank = (): ProviderProfile => ({
  id: '',
  name: '',
  provider_type: 'openrouter',
  base_url: DEFAULT_URL.openrouter,
  models: [],
  tools_enabled: true,
  is_trusted: false,
  temperature: null,
  top_p: null,
  context_length: null,
  strip_reasoning: false,
  system_prompt: null,
  created_at: '',
});

export function Providers({ highlight }: { highlight: string | null }) {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [editing, setEditing] = useState<ProviderProfile | null>(null);
  const [secret, setSecret] = useState('');
  const [confirmUntrusted, setConfirmUntrusted] = useState(false);
  const [handoffFor, setHandoffFor] = useState<ProviderView | null>(null);
  const [error, setError] = useState('');

  const refresh = () =>
    ipc
      .listProviders()
      .then(setProviders)
      .catch((e) => setError(String(e)));
  useEffect(() => void refresh(), []);

  const startNew = () => {
    setEditing(blank());
    setSecret('');
  };

  const doSave = async () => {
    if (!editing) return;
    try {
      await ipc.upsertProvider(editing, secret || null);
      setEditing(null);
      setConfirmUntrusted(false);
      setSecret('');
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const onSave = () => {
    if (!editing) return;
    // Saving a non-local provider the user hasn't marked trusted requires an
    // explicit acknowledgement (Round-2 item 18 — was tier===remote).
    if (!editing.is_trusted && !isLocal(editing.base_url)) setConfirmUntrusted(true);
    else void doSave();
  };

  const activate = async (p: ProviderView, keepContext: boolean) => {
    if (!keepContext) {
      try {
        await ipc.setActiveSession({
          session_id: '',
          cwd: '',
          current_mode: 'auto',
          available_modes: [],
        });
      } catch {
        /* non-fatal */
      }
    }
    try {
      await ipc.activateProvider(p.id);
      setHandoffFor(null);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const onActivate = async (p: ProviderView) => {
    // Context-handoff gate: switching to an untrusted, non-local provider with an
    // active session forces an explicit keep/jettison choice (Round-2 item 18).
    if (!p.is_trusted && p.network_tier !== 'local') {
      const active = await ipc.getActiveSession();
      if (active && active.session_id) {
        setHandoffFor(p);
        return;
      }
    }
    void activate(p, true);
  };

  return (
    <section className="settings-section">
      <div className="row" style={{ justifyContent: 'space-between' }}>
        <h1>Providers</h1>
        <button className="primary" onClick={startNew}>
          Add provider
        </button>
      </div>
      {error && <div className="chat-error">{error}</div>}

      <div className="provider-list">
        {providers.length === 0 && (
          <p className="muted">No profiles yet. Local Ollama is used by default.</p>
        )}
        {providers.map((p) => (
          <div
            key={p.id}
            className={`provider-row${p.id === highlight ? ' highlight' : ''}${p.active ? ' active' : ''}`}
          >
            <div>
              <div className="provider-name">
                {p.name || p.provider_type}{' '}
                <span className="status-badge">{trustBadge(p.network_tier, p.is_trusted)}</span>
                {!p.tools_enabled && <span className="status-badge">chat-only</span>}
                {p.active && <span className="status-badge">active</span>}
              </div>
              <div className="muted" style={{ fontSize: 12 }}>
                {p.provider_type} · {p.base_url}
                {p.has_secret ? ' · 🔑 key stored' : ''}
              </div>
            </div>
            <div className="row">
              {!p.active && <button onClick={() => void onActivate(p)}>Activate</button>}
              <button
                onClick={() => {
                  setEditing({ ...p });
                  setSecret('');
                }}
              >
                Edit
              </button>
              <button
                onClick={() => {
                  if (confirm(`Delete "${p.name}"?`)) void ipc.deleteProvider(p.id).then(refresh);
                }}
              >
                Delete
              </button>
            </div>
          </div>
        ))}
      </div>
      {providers.some((p) => p.active) && (
        <button className="link" onClick={() => void ipc.activateProvider(null).then(refresh)}>
          Deactivate (use Goose default config)
        </button>
      )}

      {editing && (
        <ProviderForm
          profile={editing}
          secret={secret}
          onChange={setEditing}
          onSecret={setSecret}
          onCancel={() => setEditing(null)}
          onSave={onSave}
        />
      )}

      {confirmUntrusted && editing && (
        <Modal title="This provider isn’t marked trusted">
          <p>
            Prompts, file contents, and tool outputs may be transmitted to{' '}
            <strong>{hostOf(editing.base_url)}</strong> — an unverified third party. Mark it trusted
            in the form if you control it.
          </p>
          <div className="row">
            <button className="primary" onClick={() => void doSave()}>
              I understand — save anyway
            </button>
            <button onClick={() => setConfirmUntrusted(false)}>Cancel</button>
          </div>
        </Modal>
      )}

      {handoffFor && (
        <Modal title="Send this conversation to an untrusted provider?">
          <p>
            The active session has context that would now be sent to{' '}
            <strong>{handoffFor.base_url}</strong>.
          </p>
          <div className="row">
            <button className="primary" onClick={() => void activate(handoffFor, true)}>
              Keep context (send it)
            </button>
            <button onClick={() => void activate(handoffFor, false)}>Start clean</button>
            <button onClick={() => setHandoffFor(null)}>Cancel</button>
          </div>
        </Modal>
      )}
    </section>
  );
}

function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

function ProviderForm({
  profile,
  secret,
  onChange,
  onSecret,
  onCancel,
  onSave,
}: {
  profile: ProviderProfile;
  secret: string;
  onChange: (p: ProviderProfile) => void;
  onSecret: (s: string) => void;
  onCancel: () => void;
  onSave: () => void;
}) {
  const set = (patch: Partial<ProviderProfile>) => onChange({ ...profile, ...patch });
  const needsKey = profile.provider_type !== 'ollama';
  const local = isLocal(profile.base_url);
  const ollamaLocal = profile.provider_type === 'ollama' && local;

  // Local-Ollama: offer a dropdown of installed models instead of free text (item 19).
  const [installed, setInstalled] = useState<OllamaModel[]>([]);
  useEffect(() => {
    if (!ollamaLocal) return;
    let live = true;
    void ipc
      .ollamaListModels()
      .then((m) => live && setInstalled(m))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [ollamaLocal]);

  // Context-length auto-suggest (Round-6 Feature 1) — re-resolves whenever the
  // provider type or selected model changes; never applied automatically, only
  // offered (see the suggestion row below the slider).
  const [suggested, setSuggested] = useState<number | null>(null);
  const modelsKey = profile.models.join(',');
  useEffect(() => {
    let live = true;
    setSuggested(null);
    void suggestContextLength(profile).then((v) => live && setSuggested(v));
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [profile.provider_type, modelsKey]);
  const detents = detentsFor(suggested);

  return (
    <Modal title={profile.id ? 'Edit provider' : 'Add provider'}>
      <label className="field">
        <span>Name</span>
        <input value={profile.name} onChange={(e) => set({ name: e.target.value })} />
      </label>
      <label className="field">
        <span>Type</span>
        <select
          value={profile.provider_type}
          onChange={(e) => {
            const pt = e.target.value as ProviderType;
            set({ provider_type: pt, base_url: DEFAULT_URL[pt] });
          }}
        >
          <option value="ollama">Ollama (local)</option>
          <option value="openrouter">OpenRouter</option>
          <option value="anthropic">Anthropic</option>
          <option value="openai">OpenAI</option>
          <option value="custom_openai">Custom (OpenAI-compatible)</option>
        </select>
      </label>
      <label className="field">
        <span>Base URL</span>
        <input value={profile.base_url} onChange={(e) => set({ base_url: e.target.value })} />
        <small className="muted">{trustBadge(tierOf(profile.base_url), profile.is_trusted)}</small>
      </label>

      {ollamaLocal ? (
        <label className="field">
          <span>Model</span>
          <select
            value={profile.models[0] ?? ''}
            onChange={(e) => set({ models: e.target.value ? [e.target.value] : [] })}
          >
            <option value="">(use Goose default)</option>
            {installed.map((m) => (
              <option key={m.name} value={m.name}>
                {m.name}
              </option>
            ))}
          </select>
          {installed.length === 0 && (
            <small className="muted">No installed models found — pull one in Ollama Models.</small>
          )}
        </label>
      ) : (
        <label className="field">
          <span>Models (comma-separated)</span>
          <input
            value={profile.models.join(', ')}
            onChange={(e) =>
              set({
                models: e.target.value
                  .split(',')
                  .map((m) => m.trim())
                  .filter(Boolean),
              })
            }
          />
        </label>
      )}

      {needsKey && (
        <label className="field">
          <span>API key {profile.id ? '(leave blank to keep)' : ''}</span>
          <input type="password" value={secret} onChange={(e) => onSecret(e.target.value)} />
          <small className="muted">Stored in Windows Credential Manager, never on disk.</small>
        </label>
      )}

      {!local && (
        <label className="check">
          <input
            type="checkbox"
            checked={profile.is_trusted}
            onChange={(e) => set({ is_trusted: e.target.checked })}
          />
          <span>I trust this provider (🌐 — skips the untrusted-provider warning)</span>
        </label>
      )}
      {local && <p className="muted">🔒 Local provider — always trusted.</p>}

      <label className="check">
        <input
          type="checkbox"
          checked={profile.tools_enabled}
          onChange={(e) => set({ tools_enabled: e.target.checked })}
        />
        <span>Agentic tools enabled (uncheck for a chat-only thought-partner provider)</span>
      </label>

      <label className="check">
        <input
          type="checkbox"
          checked={profile.strip_reasoning}
          onChange={(e) => set({ strip_reasoning: e.target.checked })}
        />
        <span>
          Strip reasoning from context sent on later turns (recommended for Gemma4-style local
          reasoning models; chat-only providers only)
        </span>
      </label>

      <label className="field">
        <span>Custom system prompt (optional — overrides the built-in agentic/chat default)</span>
        <textarea
          rows={4}
          value={profile.system_prompt ?? ''}
          placeholder={
            profile.tools_enabled
              ? 'Default: capable agentic assistant with scoped filesystem/shell tools…'
              : 'Default: thoughtful conversational partner, no filesystem/shell assumed…'
          }
          onChange={(e) => set({ system_prompt: e.target.value || null })}
        />
        <small className="muted">
          Sent as a hidden preamble on the first message of each new session — not visible in the
          chat bubble.
        </small>
      </label>

      {/* Per-provider sampling params (items 27/28), vertical stack so nothing overlaps. */}
      <div className="field param-slider">
        <label className="check">
          <input
            type="checkbox"
            checked={profile.temperature != null}
            onChange={(e) => set({ temperature: e.target.checked ? 0.7 : null })}
          />
          <span>Override temperature</span>
        </label>
        {profile.temperature != null && (
          <div className="row">
            <input
              type="range"
              min={0}
              max={2}
              step={0.1}
              value={profile.temperature}
              onChange={(e) => set({ temperature: Number(e.target.value) })}
            />
            <span className="status-badge">{profile.temperature.toFixed(1)}</span>
          </div>
        )}
      </div>

      <div className="field param-slider">
        <label className="check">
          <input
            type="checkbox"
            checked={profile.context_length != null}
            onChange={(e) => set({ context_length: e.target.checked ? 8192 : null })}
          />
          <span>Override context length</span>
        </label>
        {profile.context_length != null && (
          <div className="row">
            <input
              type="range"
              min={0}
              max={detents.length - 1}
              step={1}
              value={nearestCtxIndex(detents, profile.context_length)}
              onChange={(e) => set({ context_length: detents[Number(e.target.value)] })}
            />
            <span className="status-badge">{ctxLabel(profile.context_length)}</span>
          </div>
        )}
        {suggested != null && suggested !== profile.context_length && (
          <div className="row">
            <small className="muted">Detected context: {ctxLabel(suggested)}</small>
            <button className="link" onClick={() => set({ context_length: suggested })}>
              Use this
            </button>
          </div>
        )}
      </div>

      <div className="row">
        <button className="primary" onClick={onSave}>
          Save
        </button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </Modal>
  );
}

function Modal({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="modal-backdrop">
      <div className="modal">
        <h2>{title}</h2>
        {children}
      </div>
    </div>
  );
}
