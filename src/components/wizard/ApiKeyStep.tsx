import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
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
  // Not offered in this wizard's own <select> below (kept to the original 4
  // first-run options) — these labels exist only so this Record<ProviderType,
  // …> stays exhaustive now that Settings → Providers offers more types.
  fireworks: 'Fireworks',
  deepinfra: 'DeepInfra',
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
    supports_vision: false,
    system_prompt: null,
    prompt_idle_timeout_secs: null,
    parallel_slots: null,
    created_at: '',
  };
}

function hostOf(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/** Whether the step can save: a model, and a key unless the endpoint needs
    none (a custom server) or one is already saved (repair edits the existing
    card). Pure. */
export function canConnect(profile: ProviderProfile, secret: string): boolean {
  const keyOptional = profile.provider_type === 'custom_openai' || profile.id !== '';
  return !!profile.models[0]?.trim() && (keyOptional || secret.trim().length > 0);
}

/** The API-key wizard path: pick a provider, paste a key, name a model —
    reuses the exact same save/activate infra as Settings → Providers, just
    trimmed to the handful of fields a first-run newcomer actually needs.

    In repair it edits the existing default card rather than adding another
    one (#63); the key can be left blank to keep the saved one. */
export function ApiKeyStep({
  onBack,
  onNext,
  repair = false,
}: {
  onBack: () => void;
  onNext: () => void;
  repair?: boolean;
}) {
  const [profile, setProfile] = useState<ProviderProfile>(() => blankApiKeyProfile('anthropic'));
  const [secret, setSecret] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const set = (patch: Partial<ProviderProfile>) => setProfile((p) => ({ ...p, ...patch }));

  useEffect(() => {
    if (!repair) return;
    void ipc
      .listProviders()
      .then((list) => {
        const current = list.find((p) => p.active && !p.disabled_reason);
        if (current) setProfile({ ...current });
      })
      .catch(() => {});
  }, [repair]);

  const canSave = canConnect(profile, secret);

  const save = async () => {
    // A custom server nobody has vouched for gets the same warning as adding
    // one in Settings (#64).
    if (profile.provider_type === 'custom_openai' && !profile.is_trusted) {
      const ok = await confirmDialog({
        title: 'This server isn’t marked trusted',
        message: `Your chats, pasted documents and tool results will be sent to ${hostOf(profile.base_url)}. Mark it trusted in Settings → Providers if you run it yourself.`,
        confirmLabel: 'Connect anyway',
      });
      if (!ok) return;
    }
    setSaving(true);
    setError(null);
    try {
      // Checked before anything is saved: a wrong key or URL should be fixed
      // here, not discovered in the first chat.
      await ipc.testProviderDraft(profile, secret.trim() ? secret : null);
      const saved = await ipc.upsertProvider(profile, secret || null);
      await ipc.setDefaultProvider(saved.id);
      onNext();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="wizard-panel">
      <h1>{profile.id ? 'Check your provider' : 'Connect your provider'}</h1>
      <p className="muted">
        Paste an API key from an account you already have. Kitty stores it securely on this device
        and never sends it anywhere except that provider.
      </p>

      <label className="field">
        <span>Provider</span>
        <select
          value={profile.provider_type}
          disabled={!!profile.id}
          onChange={(e) => {
            const pt = e.target.value as ProviderType;
            setProfile(blankApiKeyProfile(pt));
            setSecret('');
          }}
        >
          {(
            (profile.id &&
            !['anthropic', 'openai', 'openrouter', 'custom_openai'].includes(profile.provider_type)
              ? [profile.provider_type]
              : []) as ProviderType[]
          )
            .concat(['anthropic', 'openai', 'openrouter', 'custom_openai'])
            .map((t) => (
              <option key={t} value={t}>
                {TYPE_LABEL[t]}
              </option>
            ))}
        </select>
      </label>

      {(profile.provider_type === 'custom_openai' || profile.provider_type === 'ollama') && (
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
          placeholder={
            profile.id
              ? 'Leave blank to keep the saved key'
              : profile.provider_type === 'custom_openai'
                ? 'Optional'
                : 'sk-…'
          }
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
