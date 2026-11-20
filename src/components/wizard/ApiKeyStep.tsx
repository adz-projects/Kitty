import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import type { ProviderProfile, ProviderType } from '@/lib/types';
import { DEFAULT_URL } from '@/lib/provider_defaults';
import { initialChecks } from '@/components/settings/providers/providerUtils';
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
    models: [],
    enabled_models: [],
    model_ids: {},
    vision_models: [],
    context_lengths: {},
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

/** Whether the step can save: a key unless the endpoint needs none (a
    custom server) or one is already saved (repair edits the existing
    provider). The model list is fetched on Connect. Pure. */
export function canConnect(profile: ProviderProfile, secret: string): boolean {
  const keyOptional = profile.provider_type === 'custom_openai' || profile.id !== '';
  return keyOptional || secret.trim().length > 0;
}

/** The API-key wizard path: pick a provider and paste a key. Connect checks
    the key, fetches the provider's models, checks them the way Settings →
    Providers does (a short list all, a long one just the suggested model)
    and makes the suggested model — or one typed in — the default.

    In repair it edits the provider of the default model rather than adding
    another one (#63); the key can be left blank to keep the saved one. */
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
  const [typedModel, setTypedModel] = useState('');

  const set = (patch: Partial<ProviderProfile>) => setProfile((p) => ({ ...p, ...patch }));

  useEffect(() => {
    if (!repair) return;
    void Promise.all([ipc.listProviders(), ipc.listProviderAccounts()])
      .then(([cards, accounts]) => {
        const current = cards.find((p) => p.active && !p.disabled_reason);
        const account = accounts.find((a) => a.id === current?.provider_id);
        if (account) {
          // eslint-disable-next-line @typescript-eslint/no-unused-vars
          const { network_tier, has_secret, requires_key, model_infos, ...stored } = account;
          setProfile(stored);
        }
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
      const key = secret.trim() ? secret : null;
      await ipc.testProviderDraft(profile, key);
      const typed = typedModel.trim();
      let listed: string[] = [];
      try {
        listed = await ipc.discoverProviderModels(
          profile.provider_type,
          profile.base_url,
          profile.id || null,
          key
        );
      } catch (e) {
        // A server that lists nothing still works with a model named here.
        if (!typed) throw e;
      }
      const models = typed && !listed.includes(typed) ? [...listed, typed] : listed;
      const suggested =
        typed ||
        (await ipc
          .suggestProviderModel({ ...profile, models, enabled_models: models })
          .catch(() => null));
      const checked = initialChecks(models, profile.enabled_models, suggested);
      const enabled = suggested && !checked.includes(suggested) ? [...checked, suggested] : checked;
      const saved = await ipc.upsertProvider({ ...profile, models, enabled_models: enabled }, key);
      const card = suggested ? saved.model_ids[suggested] : undefined;
      if (card) await ipc.setDefaultProvider(card);
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
        <span>Model (optional)</span>
        <input
          value={typedModel}
          placeholder="Kitty picks one from the provider's list"
          onChange={(e) => setTypedModel(e.target.value)}
        />
        <small className="muted">
          Kitty fetches the provider&apos;s models and starts you on a strong, sensibly priced one.
          Check more models, or choose another default, in Settings → Providers.
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
