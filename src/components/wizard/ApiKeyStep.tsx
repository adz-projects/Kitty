import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { ProviderProfile, ProviderType } from '@/lib/types';
import { DEFAULT_URL, DEFAULT_MODEL } from '@/lib/provider_defaults';
import { ErrorDetail } from '@/components/shared/ErrorDetail';

const TYPE_LABEL: Record<ProviderType, string> = {
  anthropic: 'Anthropic (Claude)',
  openai: 'OpenAI (ChatGPT)',
  openrouter: 'OpenRouter',
  custom_openai: 'Custom (OpenAI-compatible)',
  ollama: 'Ollama (self-hosted)',
  local: 'On this device',
};

// First-party types get the trusted (globe) badge immediately — a newcomer
// who just pasted a key from the provider's own console has no reason to see
// a scary "untrusted" warning on day one. Custom endpoints stay untrusted by
// default, same as adding one from Settings → Providers.
const FIRST_PARTY: ProviderType[] = ['anthropic', 'openai', 'openrouter'];

function blankApiKeyProfile(type: ProviderType): ProviderProfile {
  return {
    id: '',
    name: TYPE_LABEL[type],
    provider_type: type,
    base_url: DEFAULT_URL[type],
    models: DEFAULT_MODEL[type] ? [DEFAULT_MODEL[type]!] : [],
    is_trusted: FIRST_PARTY.includes(type),
    temperature: null,
    top_p: null,
    top_k: null,
    min_p: null,
    presence_penalty: null,
    frequency_penalty: null,
    max_tokens: null,
    context_length: null,
    strip_reasoning: false,
    system_prompt: null,
    prompt_idle_timeout_secs: null,
    parallel_slots: null,
    created_at: '',
  };
}

/** The API-key wizard path: pick a provider, paste a key, name a model —
    reuses the exact same save/activate infra as Settings → Providers, just
    trimmed to the handful of fields a first-run newcomer actually needs. */
export function ApiKeyStep({ onBack, onNext }: { onBack: () => void; onNext: () => void }) {
  const [profile, setProfile] = useState<ProviderProfile>(() => blankApiKeyProfile('anthropic'));
  const [secret, setSecret] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const set = (patch: Partial<ProviderProfile>) => setProfile((p) => ({ ...p, ...patch }));

  const canSave =
    profile.models[0]?.trim() && (secret.trim() || profile.provider_type === 'custom_openai');

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      const saved = await ipc.upsertProvider(profile, secret || null);
      await ipc.activateProvider(saved.id);
      onNext();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="wizard-panel">
      <h1>Connect your provider</h1>
      <p className="muted">
        Paste an API key from an account you already have. Kitty stores it securely on this computer
        and never sends it anywhere except that provider.
      </p>

      <label className="field">
        <span>Provider</span>
        <select
          value={profile.provider_type}
          onChange={(e) => {
            const pt = e.target.value as ProviderType;
            setProfile(blankApiKeyProfile(pt));
            setSecret('');
          }}
        >
          {(['anthropic', 'openai', 'openrouter', 'custom_openai'] as ProviderType[]).map((t) => (
            <option key={t} value={t}>
              {TYPE_LABEL[t]}
            </option>
          ))}
        </select>
      </label>

      {profile.provider_type === 'custom_openai' && (
        <label className="field">
          <span>Base URL</span>
          <input value={profile.base_url} onChange={(e) => set({ base_url: e.target.value })} />
        </label>
      )}

      <label className="field">
        <span>API key</span>
        <input
          type="password"
          autoComplete="off"
          value={secret}
          placeholder={profile.provider_type === 'custom_openai' ? 'Optional' : 'sk-…'}
          onChange={(e) => setSecret(e.target.value)}
        />
      </label>

      <label className="field">
        <span>Model</span>
        <input
          value={profile.models[0] ?? ''}
          onChange={(e) => set({ models: [e.target.value] })}
        />
        <small className="muted">
          You can change this any time from Settings → Providers once you're chatting.
        </small>
      </label>

      {error && <ErrorDetail summary="Couldn't save that provider." raw={error} />}

      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        <button className="primary" disabled={!canSave || saving} onClick={() => void save()}>
          {saving ? 'Connecting…' : 'Connect'}
        </button>
      </div>
    </section>
  );
}
