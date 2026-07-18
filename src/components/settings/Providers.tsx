import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { Modal } from '@/components/shared/Modal';
import type {
  NetworkTier,
  OllamaModel,
  OpenRouterCredits,
  ProviderProfile,
  ProviderType,
  ProviderView,
} from '@/lib/types';
import { TrustBadge } from '@/lib/provider_trust';
import {
  ANTHROPIC_CONTEXT_TABLE,
  CUSTOM_OPENAI_CONTEXT_TABLE,
  lookupContextLength,
} from '@/lib/context_length_table';
import { LockIcon } from '@/components/icons/LockIcon';
import { GlobeIcon } from '@/components/icons/GlobeIcon';
import { DEFAULT_URL } from '@/lib/provider_defaults';

/** One-click quick-start presets for NPU/hybrid-NPU+GPU local inference —
    both are just `custom_openai` profiles pointed at a well-known local
    server, since Kitty's `custom_openai` type already handles the whole
    request/env/trust/network-tier path generically (no backend changes
    needed). Ports are the best-documented current defaults, but both
    projects have shipped different defaults across versions (Foundry
    Local: 5272 vs an older 5273; Lemonade: 13305 vs an older 8000) — the
    help text below says so rather than presenting false confidence. */
const LOCAL_NPU_PRESETS: { label: string; name: string; baseUrl: string; note: string }[] = [
  {
    label: 'Foundry Local',
    name: 'Foundry Local',
    baseUrl: 'http://localhost:5272/v1',
    note: "Microsoft's vendor-neutral local server — auto-detects AMD/Intel/Qualcomm NPU, GPU, or CPU. Install: winget install Microsoft.FoundryLocal. If this port doesn't connect, run `foundry service status` to find the real one.",
  },
  {
    label: 'Lemonade Server (AMD)',
    name: 'Lemonade Server',
    baseUrl: 'http://localhost:13305/api/v1',
    note: "AMD's own local server — purpose-built NPU+GPU hybrid scheduling on Ryzen AI (XDNA), may outperform a generic execution-provider abstraction on that hardware specifically. If this port doesn't connect, check Lemonade's own settings for the port your installed version uses.",
  },
];

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
  is_trusted: false,
  temperature: null,
  top_p: null,
  context_length: null,
  strip_reasoning: false,
  system_prompt: null,
  prompt_idle_timeout_secs: null,
  created_at: '',
});

export function Providers({ highlight }: { highlight: string | null }) {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [editing, setEditing] = useState<ProviderProfile | null>(null);
  const [secret, setSecret] = useState('');
  const [confirmUntrusted, setConfirmUntrusted] = useState(false);
  const [handoffFor, setHandoffFor] = useState<ProviderView | null>(null);
  const [error, setError] = useState('');
  const [credits, setCredits] = useState<
    Record<
      string,
      | { status: 'loading' }
      | { status: 'error'; message: string }
      | { status: 'ok'; data: OpenRouterCredits }
    >
  >({});
  // Whether local inference is opted into at all (wizard redesign) — hides
  // the Ollama option from the type picker when the user picked the
  // API-key path and hasn't re-enabled it from Advanced.
  const [ollamaEnabled, setOllamaEnabled] = useState(true);

  const refresh = () =>
    ipc
      .listProviders()
      .then(setProviders)
      .catch((e) => setError(String(e)));
  useEffect(() => void refresh(), []);
  useEffect(() => {
    void ipc.getConfig().then((c) => setOllamaEnabled(c.ollama_enabled));
  }, []);

  const checkCredits = async (id: string) => {
    setCredits((prev) => ({ ...prev, [id]: { status: 'loading' } }));
    try {
      const data = await ipc.openrouterCredits(id);
      setCredits((prev) => ({ ...prev, [id]: { status: 'ok', data } }));
    } catch (e) {
      setCredits((prev) => ({ ...prev, [id]: { status: 'error', message: String(e) } }));
    }
  };

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
          thinking_effort: null,
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
                <span className="status-badge">
                  <TrustBadge tier={p.network_tier} isTrusted={p.is_trusted} />
                </span>
                {p.active && <span className="status-badge">active</span>}
              </div>
              <div className="muted" style={{ fontSize: 12 }}>
                {p.provider_type} · {p.base_url}
                {p.has_secret ? ' · 🔑 key stored' : ''}
              </div>
              {p.provider_type === 'openrouter' && p.has_secret && (
                <div className="muted" style={{ fontSize: 12 }}>
                  {!credits[p.id] && (
                    <button className="link" onClick={() => void checkCredits(p.id)}>
                      Check credits
                    </button>
                  )}
                  {credits[p.id]?.status === 'loading' && 'Checking…'}
                  {credits[p.id]?.status === 'error' && (
                    <>
                      {(credits[p.id] as { status: 'error'; message: string }).message}{' '}
                      <button className="link" onClick={() => void checkCredits(p.id)}>
                        Retry
                      </button>
                    </>
                  )}
                  {credits[p.id]?.status === 'ok' &&
                    (() => {
                      const d = (credits[p.id] as { status: 'ok'; data: OpenRouterCredits }).data;
                      const remaining =
                        d.limit_remaining != null
                          ? `$${d.limit_remaining.toFixed(2)} remaining`
                          : d.is_free_tier
                            ? 'Free tier'
                            : 'No spend limit set';
                      return (
                        <>
                          {remaining} · ${d.usage.toFixed(2)} used
                          {d.limit != null ? ` of $${d.limit.toFixed(2)}` : ''}{' '}
                          <button className="link" onClick={() => void checkCredits(p.id)}>
                            Refresh
                          </button>
                        </>
                      );
                    })()}
                </div>
              )}
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
          ollamaEnabled={ollamaEnabled}
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
  ollamaEnabled,
  onChange,
  onSecret,
  onCancel,
  onSave,
}: {
  profile: ProviderProfile;
  secret: string;
  ollamaEnabled: boolean;
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
  const [advancedOpen, setAdvancedOpen] = useState(false);

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
          {ollamaEnabled && <option value="ollama">Ollama (local)</option>}
          <option value="openrouter">OpenRouter</option>
          <option value="anthropic">Anthropic</option>
          <option value="openai">OpenAI</option>
          <option value="custom_openai">Custom (OpenAI-compatible)</option>
        </select>
      </label>
      <div className="field">
        <span>Or quick-start a local NPU/hybrid-inference server</span>
        <div className="row">
          {LOCAL_NPU_PRESETS.map((preset) => (
            <button
              key={preset.label}
              type="button"
              title={preset.note}
              onClick={() =>
                set({ provider_type: 'custom_openai', base_url: preset.baseUrl, name: preset.name })
              }
            >
              {preset.label}
            </button>
          ))}
        </div>
        <small className="muted">
          Both fill in a `custom_openai` profile — no separate provider type needed. Ports vary by
          installed version; hover a button for the exact caveat, and check the server's own
          status/settings if the preset URL doesn't connect.
        </small>
      </div>
      <label className="field">
        <span>Base URL</span>
        <input value={profile.base_url} onChange={(e) => set({ base_url: e.target.value })} />
        <small className="muted trust-note">
          <TrustBadge tier={tierOf(profile.base_url)} isTrusted={profile.is_trusted} />
        </small>
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

      {local && (
        <p className="muted trust-note">
          <LockIcon /> Local provider — always trusted.
        </p>
      )}

      <button
        type="button"
        className="disclosure-toggle"
        onClick={() => setAdvancedOpen((o) => !o)}
      >
        {advancedOpen ? '▾' : '▸'} Advanced
      </button>
      {/* Explicit conditional render, not native <details> collapse — this
          WebView2/Chromium build doesn't actually hide non-open <details>
          content (confirmed live: even a bare, class-free <details> child
          stays visible while closed), so visibility can't be left to CSS. */}
      {advancedOpen && (
        <div className="provider-advanced-body">
          {!local && (
            <label className="check">
              <input
                type="checkbox"
                checked={profile.is_trusted}
                onChange={(e) => set({ is_trusted: e.target.checked })}
              />
              <span>
                I trust this provider (<GlobeIcon /> skips the untrusted-provider warning)
              </span>
            </label>
          )}

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
            <span>
              Custom system prompt (optional — overrides the built-in agentic/chat default)
            </span>
            <textarea
              rows={4}
              value={profile.system_prompt ?? ''}
              placeholder="Default: a built-in prompt matching the session's current chat/agent mode…"
              onChange={(e) => set({ system_prompt: e.target.value || null })}
            />
            <small className="muted">
              Sent as a hidden preamble on the first message of each new session — not visible in
              the chat bubble.
            </small>
          </label>

          <label className="field">
            <span>Response timeout (seconds, optional — default 300)</span>
            <input
              type="number"
              min={30}
              step={30}
              value={profile.prompt_idle_timeout_secs ?? ''}
              placeholder="300"
              onChange={(e) =>
                set({
                  prompt_idle_timeout_secs: e.target.value ? Number(e.target.value) : null,
                })
              }
            />
            <small className="muted">
              How long Kitty waits for this provider to respond (or keep streaming) before giving
              up. Raise this for a model that legitimately has long gaps between updates (e.g. a
              slow Tailscale-hosted host); lower it if a long silence there usually means it&rsquo;s
              stuck.
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
        </div>
      )}

      <div className="row">
        <button className="primary" onClick={onSave}>
          Save
        </button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </Modal>
  );
}
