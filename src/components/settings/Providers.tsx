import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { NetworkTier, ProviderProfile, ProviderType, ProviderView } from '@/lib/types';

const TIER_BADGE: Record<NetworkTier, string> = {
  local: '🖥 local',
  personal: '🔒 private network',
  remote: '☁ remote',
};

const DEFAULT_URL: Record<ProviderType, string> = {
  ollama: 'http://localhost:11434',
  openrouter: 'https://openrouter.ai/api/v1',
  anthropic: 'https://api.anthropic.com',
  openai: 'https://api.openai.com/v1',
  custom_openai: '',
};

/** Client-side mirror of providers::network_tier_for (for live form preview). */
function tierOf(url: string): NetworkTier {
  const host = (url.split('://').pop() ?? '').split('/')[0].split('@').pop() ?? '';
  const h = host.split(':')[0].toLowerCase();
  if (!h || h === 'localhost' || h === '127.0.0.1' || h === '::1') return 'local';
  if (h.endsWith('.ts.net')) return 'personal';
  const o = h.split('.').map(Number);
  if (o.length === 4 && o[0] === 100 && o[1] >= 64 && o[1] <= 127) return 'personal';
  return 'remote';
}

const blank = (): ProviderProfile => ({
  id: '',
  name: '',
  provider_type: 'openrouter',
  base_url: DEFAULT_URL.openrouter,
  models: [],
  tools_enabled: true,
  created_at: '',
});

export function Providers({ highlight }: { highlight: string | null }) {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [editing, setEditing] = useState<ProviderProfile | null>(null);
  const [secret, setSecret] = useState('');
  const [confirmRemote, setConfirmRemote] = useState(false);
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
      setConfirmRemote(false);
      setSecret('');
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const onSave = () => {
    if (!editing) return;
    // Adding/editing a remote-tier profile requires an explicit acknowledgement.
    if (tierOf(editing.base_url) === 'remote') setConfirmRemote(true);
    else void doSave();
  };

  const activate = async (p: ProviderView, keepContext: boolean) => {
    if (!keepContext) {
      // Start clean: drop the handed-off session so windows begin fresh.
      // (No dedicated clear command; overwrite with an empty active session.)
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
    // Context-handoff gate: switching to a remote provider with an active session
    // forces an explicit keep/jettison choice, every time (CLAUDE.md Phase 5).
    if (p.network_tier === 'remote') {
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
                <span className="status-badge">{TIER_BADGE[p.network_tier]}</span>
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

      {confirmRemote && editing && (
        <Modal title="This is a remote provider">
          <p>
            Prompts, file contents, and tool outputs sent to this session may be transmitted to{' '}
            <strong>
              {tierOf(editing.base_url) === 'remote' ? new URL(editing.base_url).host : ''}
            </strong>{' '}
            — a third party.
          </p>
          <div className="row">
            <button className="primary" onClick={() => void doSave()}>
              I understand
            </button>
            <button onClick={() => setConfirmRemote(false)}>Cancel</button>
          </div>
        </Modal>
      )}

      {handoffFor && (
        <Modal title="Send this conversation to a remote provider?">
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
        <small className="muted">Tier: {TIER_BADGE[tierOf(profile.base_url)]}</small>
      </label>
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
      {needsKey && (
        <label className="field">
          <span>API key {profile.id ? '(leave blank to keep)' : ''}</span>
          <input type="password" value={secret} onChange={(e) => onSecret(e.target.value)} />
          <small className="muted">Stored in Windows Credential Manager, never on disk.</small>
        </label>
      )}
      <label className="check">
        <input
          type="checkbox"
          checked={profile.tools_enabled}
          onChange={(e) => set({ tools_enabled: e.target.checked })}
        />
        <span>Tools enabled (uncheck for a chat-only thought-partner provider)</span>
      </label>
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
