import { useEffect, useMemo, useState } from 'react';
import { Modal } from '@/components/shared/Modal';
import type { ModelInfo, ProviderProfile, ProviderType, Setup } from '@/lib/types';
import { TrustBadge } from '@/lib/provider_trust';
import { LockIcon } from '@/components/icons/LockIcon';
import { GlobeIcon } from '@/components/icons/GlobeIcon';
import { DEFAULT_URL } from '@/lib/provider_defaults';
import { ipc } from '@/lib/ipc';
import { modelTitle, priceHint } from '@/lib/models';
import { canSaveProvider, hasFixedEndpoint, initialChecks, isLocal, tierOf } from './providerUtils';
import { setupsUsing } from '../setups/setupUtils';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import { isAndroid } from '@/lib/platform';

/** The service's own name, offered as a new provider's name. */
const TYPE_NAME: Partial<Record<ProviderType, string>> = {
  openrouter: 'OpenRouter',
  anthropic: 'Anthropic',
  openai: 'OpenAI',
  fireworks: 'Fireworks',
  deepinfra: 'DeepInfra',
  ollama: 'Ollama',
};

/** A ticked box: check every listed model. */
function CheckAllIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <rect x="2" y="2" width="12" height="12" rx="2" stroke="currentColor" strokeWidth="1.4" />
      <path
        d="M5 8.2l2 2 4-4.4"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** An empty box: uncheck every listed model. */
function UncheckAllIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <rect x="2" y="2" width="12" height="12" rx="2" stroke="currentColor" strokeWidth="1.4" />
    </svg>
  );
}

/** Where an API key actually lives — the OS store differs per platform. */
const SECRET_STORE_NOTE = isAndroid()
  ? 'Encrypted with a key held in Android’s secure keystore, never stored in plain text.'
  : 'Stored in Windows Credential Manager, never on disk.';

export function ProviderForm({
  profile,
  secret,
  setups,
  onChange,
  onSecret,
  onCancel,
  onSave,
}: {
  profile: ProviderProfile;
  secret: string;
  /** Every setup, to mark (and guard) the models they run on. */
  setups: Setup[];
  onChange: (p: ProviderProfile) => void;
  onSecret: (s: string) => void;
  onCancel: () => void;
  onSave: () => void;
}) {
  const set = (patch: Partial<ProviderProfile>) => onChange({ ...profile, ...patch });
  const local = isLocal(profile.base_url);
  // An Ollama server takes no API key; a custom OpenAI-compatible server may
  // or may not (#64), so its key is optional.
  const needsKey = profile.provider_type !== 'ollama' && profile.provider_type !== 'local';
  const keyOptional = profile.provider_type === 'custom_openai';

  // "Test connection" for every type (#66): the card as edited, with the
  // typed key (or the saved one when the field is blank).
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; text: string } | null>(null);
  useEffect(() => setTestResult(null), [profile.provider_type, profile.base_url, secret]);
  const testConnection = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      await ipc.testProviderDraft(profile, secret.trim() ? secret : null);
      setTestResult({ ok: true, text: 'Connected.' });
    } catch (e) {
      setTestResult({ ok: false, text: String(e) });
    } finally {
      setTesting(false);
    }
  };

  const [advancedOpen, setAdvancedOpen] = useState(false);

  // Hosted services sit at a fixed, well-known endpoint: no URL field.
  const fixedEndpoint = hasFixedEndpoint(profile.provider_type);

  // The model list: fetched from the provider, then described and ranked
  // against OpenRouter's catalog (`ai_models`), with the price band of each.
  const [fetching, setFetching] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  const [described, setDescribed] = useState<ModelInfo[]>([]);
  const [filter, setFilter] = useState('');
  const [typedModel, setTypedModel] = useState('');
  const modelList = profile.models.join('\n');
  useEffect(() => {
    let live = true;
    if (profile.models.length === 0) {
      setDescribed([]);
      return;
    }
    // Marks left out, so `vision` below is what the catalog says on its own.
    void ipc
      .describeProviderModels({ ...profile, vision_models: [] })
      .then((list) => live && setDescribed(list))
      .catch(() => live && setDescribed([]));
    return () => {
      live = false;
    };
    // Only a new list (or provider) changes the descriptions.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [modelList, profile.provider_type, profile.base_url]);
  // A different service or key: the old list belonged to another endpoint.
  useEffect(() => setFetchError(null), [profile.provider_type, secret]);

  const fetchModels = async () => {
    setFetching(true);
    setFetchError(null);
    try {
      const list = await ipc.discoverProviderModels(
        profile.provider_type,
        profile.base_url,
        profile.id || null,
        secret.trim() || null
      );
      // Kept models still listed stay; a hand-typed model the server does
      // not list stays too (it may serve models it does not advertise).
      const typed = fixedEndpoint
        ? []
        : profile.models.filter((m) => !list.includes(m) && profile.enabled_models.includes(m));
      const models = [...list, ...typed];
      const suggested = await ipc
        .suggestProviderModel({ ...profile, models, enabled_models: models })
        .catch(() => null);
      onChange({
        ...profile,
        models,
        enabled_models: initialChecks(models, profile.enabled_models, suggested),
        vision_models: profile.vision_models.filter((m) => models.includes(m)),
      });
    } catch (e) {
      setFetchError(String(e));
    } finally {
      setFetching(false);
    }
  };

  const addTypedModel = () => {
    const m = typedModel.trim();
    if (!m) return;
    setTypedModel('');
    if (profile.models.includes(m)) {
      if (!profile.enabled_models.includes(m)) {
        set({ enabled_models: [...profile.enabled_models, m] });
      }
      return;
    }
    set({ models: [...profile.models, m], enabled_models: [...profile.enabled_models, m] });
  };

  // Described models in rank order; anything not described yet (the
  // description is a round trip behind) at the end, as its id.
  const ranked = useMemo(() => {
    const known = new Set(described.map((d) => d.model));
    return [
      ...described.filter((d) => profile.models.includes(d.model)),
      ...profile.models
        .filter((m) => !known.has(m))
        .map((m): ModelInfo => ({
          provider_id: profile.id,
          provider_name: profile.name,
          provider_type: profile.provider_type,
          model: m,
          card_id: null,
          name: m,
          enabled: true,
          vision: false,
          in_catalog: false,
          created: null,
          price_out_per_m: null,
          price_in_per_m: null,
          price_tier: null,
          intelligence: null,
          context_length: null,
        })),
    ];
  }, [described, profile.models, profile.id, profile.name, profile.provider_type]);
  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return ranked;
    return ranked.filter(
      (m) => m.name.toLowerCase().includes(q) || m.model.toLowerCase().includes(q)
    );
  }, [ranked, filter]);

  const isOn = (model: string) => profile.enabled_models.includes(model);
  const usedBy = (model: string) => (profile.id ? setupsUsing(setups, profile.id, model) : []);
  // Unchecking a model a setup runs on leaves that setup unusable: ask first.
  const confirmUncheck = async (models: string[]) => {
    const names = [...new Set(models.flatMap(usedBy))];
    if (names.length === 0) return true;
    return confirmDialog({
      title: models.length === 1 ? `Uncheck ${models[0]}?` : 'Uncheck models in use?',
      message: `${names.join(', ')} ${names.length === 1 ? 'runs' : 'run'} on ${
        models.length === 1 ? 'it' : 'them'
      }, and can't be used until you choose another model for ${
        names.length === 1 ? 'it' : 'them'
      } under Setups.`,
      confirmLabel: 'Uncheck',
    });
  };
  const toggle = async (model: string, on: boolean) => {
    if (!on && !(await confirmUncheck([model]))) return;
    set({
      enabled_models: on
        ? [...profile.enabled_models, model]
        : profile.enabled_models.filter((m) => m !== model),
    });
  };
  // Check / uncheck every model the filter shows (all of them when empty).
  const setAll = async (on: boolean) => {
    const ids = new Set(shown.map((m) => m.model));
    if (!on && !(await confirmUncheck([...ids].filter(isOn)))) return;
    const rest = profile.enabled_models.filter((m) => !ids.has(m));
    set({ enabled_models: on ? [...rest, ...ids] : rest });
  };
  const markVision = (model: string, on: boolean) =>
    set({
      vision_models: on
        ? [...profile.vision_models, model]
        : profile.vision_models.filter((m) => m !== model),
    });
  const enabledCount = profile.models.filter(isOn).length;

  // A provider is saved once at least one of its models is checked.
  const canSave = canSaveProvider(profile);

  return (
    <Modal title={profile.id ? 'Edit provider' : 'Add provider'} onClose={onCancel}>
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
            // Another service: another model list. A name the user has not
            // changed follows the service.
            const followName = !profile.name || profile.name === TYPE_NAME[profile.provider_type];
            set({
              provider_type: pt,
              base_url: DEFAULT_URL[pt],
              name: followName ? (TYPE_NAME[pt] ?? '') : profile.name,
              models: [],
              enabled_models: [],
              vision_models: [],
            });
          }}
        >
          {/* Retired (decision #65): only shown so an old card still says
              what it is; it can't be chosen for a new one. */}
          {profile.provider_type === 'local' && (
            <option value="local" disabled>
              On this device (no longer supported)
            </option>
          )}
          <option value="openrouter">OpenRouter</option>
          <option value="anthropic">Anthropic</option>
          <option value="openai">OpenAI</option>
          <option value="fireworks">Fireworks</option>
          <option value="deepinfra">DeepInfra</option>
          <option value="custom_openai">Custom (OpenAI-compatible)</option>
          {/* Not offered for new providers — Kitty no longer runs Ollama
              itself — but an existing ollama-type profile (pointing at a
              server the user runs) stays fully editable, so its own type
              must still appear as an option or the select would silently
              show a different type as "selected" while saving. */}
          {profile.provider_type === 'ollama' && (
            <option value="ollama">Ollama (self-hosted)</option>
          )}
        </select>
      </label>
      {/* Hosted services sit at fixed, well-known endpoints the user never
          needs to see or edit. Still shown for local/ollama/custom_openai,
          which genuinely need an editable endpoint. */}
      {!fixedEndpoint && (
        <label className="field">
          <span>Base URL</span>
          <input value={profile.base_url} onChange={(e) => set({ base_url: e.target.value })} />
          <small className="muted trust-note">
            <TrustBadge tier={tierOf(profile.base_url)} isTrusted={profile.is_trusted} />
          </small>
          {tierOf(profile.base_url) === 'personal' && (
            <small className="muted">
              This is a Tailscale address, so one URL works both at home and away: Kitty
              automatically tries a direct LAN connection first when you&rsquo;re on the same
              network as the server, and falls back to routing over Tailscale otherwise — no need to
              switch URLs manually.
            </small>
          )}
        </label>
      )}

      {needsKey && (
        <label className="field">
          <span>
            API key{' '}
            {profile.id
              ? '(leave blank to keep)'
              : keyOptional
                ? '(optional — only if your server needs one)'
                : ''}
          </span>
          <input type="password" value={secret} onChange={(e) => onSecret(e.target.value)} />
          <small className="muted">{SECRET_STORE_NOTE}</small>
        </label>
      )}

      <div className="field">
        <div className="model-list-head">
          <span>
            Models
            {profile.models.length > 0 && (
              <span className="muted">
                {' '}
                — {enabledCount} of {profile.models.length} checked
              </span>
            )}
          </span>
          <button
            type="button"
            onClick={() => void fetchModels()}
            disabled={fetching || (needsKey && !keyOptional && !secret.trim() && !profile.id)}
          >
            {fetching
              ? 'Fetching…'
              : profile.models.length
                ? 'Fetch again'
                : needsKey && !keyOptional
                  ? 'Check key & fetch models'
                  : 'Fetch models'}
          </button>
        </div>
        {fetchError && (
          <small className="chat-error" role="alert">
            {fetchError}
          </small>
        )}
        {profile.models.length === 0 ? (
          <small className="muted">
            Fetch the provider&apos;s models, then check the ones you may want to use. Setups choose
            from the checked models.
          </small>
        ) : (
          <>
            <div className="row model-list-tools">
              <input
                type="search"
                aria-label="Filter models"
                placeholder="Filter models"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
              />
              <button
                type="button"
                className="icon-button"
                onClick={() => void setAll(true)}
                aria-label={filter.trim() ? 'Check all shown models' : 'Check all models'}
                title={filter.trim() ? 'Check all shown models' : 'Check all models'}
              >
                <CheckAllIcon />
              </button>
              <button
                type="button"
                className="icon-button"
                onClick={() => void setAll(false)}
                aria-label={filter.trim() ? 'Uncheck all shown models' : 'Uncheck all models'}
                title={filter.trim() ? 'Uncheck all shown models' : 'Uncheck all models'}
              >
                <UncheckAllIcon />
              </button>
            </div>
            <ul className="model-checklist" aria-label="Checked models, for setups">
              {shown.map((m) => {
                const id = `prov-model-${m.model}`;
                const marked = profile.vision_models.includes(m.model);
                return (
                  <li key={m.model}>
                    <input
                      id={id}
                      type="checkbox"
                      checked={isOn(m.model)}
                      onChange={(e) => void toggle(m.model, e.target.checked)}
                    />
                    <label htmlFor={id} className="model-name" title={modelTitle(m)}>
                      {m.name}
                      {m.name !== m.model && <span className="model-id">{m.model}</span>}
                    </label>
                    {usedBy(m.model).length > 0 && (
                      <span
                        className="model-used-by"
                        title={`Used by ${usedBy(m.model).join(', ')}`}
                      >
                        in use
                      </span>
                    )}
                    {m.price_tier && (
                      <span className="price-tier" title={priceHint(m) ?? undefined}>
                        {m.price_tier}
                      </span>
                    )}
                    {m.vision ? (
                      <span className="model-vision" title="This model reads images">
                        vision
                      </span>
                    ) : (
                      <label className="vision-mark" title="Mark if this model reads images">
                        <input
                          type="checkbox"
                          checked={marked}
                          onChange={(e) => markVision(m.model, e.target.checked)}
                        />{' '}
                        vision
                      </label>
                    )}
                  </li>
                );
              })}
              {shown.length === 0 && <li className="muted">No model matches the filter.</li>}
            </ul>
            <small className="muted">
              Ordered by intelligence and how recent each model is. $ is up to $3 per million output
              tokens, $$ up to $8, $$$ more.
            </small>
          </>
        )}
        {!fixedEndpoint && (
          <div className="row model-list-tools">
            <input
              aria-label="Add a model by name"
              placeholder="Add a model by name"
              value={typedModel}
              onChange={(e) => setTypedModel(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault();
                  addTypedModel();
                }
              }}
            />
            <button type="button" onClick={addTypedModel} disabled={!typedModel.trim()}>
              Add
            </button>
          </div>
        )}
      </div>

      <div className="row">
        <button type="button" onClick={() => void testConnection()} disabled={testing}>
          {testing ? 'Testing…' : 'Test connection'}
        </button>
        {testResult && (
          <small className={testResult.ok ? 'muted' : 'error'} role="status">
            {testResult.text}
          </small>
        )}
      </div>

      {local && (
        <p className="muted trust-note">
          <LockIcon /> Local provider — always trusted.
        </p>
      )}

      {/* Moved above Advanced (release-fixes-2) — trust is a decision worth
          seeing up front, not buried in a collapsed section; copy shortened
          to drop the mechanism explanation. */}
      {!local && (
        <label className="check">
          <input
            type="checkbox"
            checked={profile.is_trusted}
            onChange={(e) => set({ is_trusted: e.target.checked })}
          />
          <span>
            <GlobeIcon /> I trust this provider
          </span>
        </label>
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
          <label className="field">
            <span>Can these models use tools?</span>
            <select
              value={
                profile.supports_tools == null ? 'auto' : profile.supports_tools ? 'yes' : 'no'
              }
              onChange={(e) =>
                set({
                  supports_tools: e.target.value === 'auto' ? null : e.target.value === 'yes',
                })
              }
            >
              <option value="auto">Detect automatically</option>
              <option value="yes">Yes</option>
              <option value="no">No</option>
            </select>
            <small className="muted">
              Decides whether dropped files are handed over as paths (a model with tools opens them
              itself) or pasted into the message. Kitty detects it for known models; set it for a
              self-hosted or renamed one.
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
              How long Kitty waits before giving up on a reply. Raise it for slow or
              Tailscale-hosted models; lower it if a stall usually means it&rsquo;s stuck.
            </small>
          </label>

          <label className="field">
            <span>Parallel slots (optional — for llama-server prompt-cache pinning)</span>
            <input
              type="number"
              min={1}
              step={1}
              value={profile.parallel_slots ?? ''}
              placeholder="Not set — no slot pinning"
              onChange={(e) =>
                set({
                  parallel_slots: e.target.value ? Number(e.target.value) : null,
                })
              }
            />
            <small className="muted">
              Must exactly match this llama-server&rsquo;s own <code>--parallel</code>/
              <code>-np</code> value — pins each session to one KV-cache slot so the prompt cache
              actually hits. Leave unset for Ollama or anything else.
            </small>
          </label>
        </div>
      )}

      <div className="row">
        <button className="primary" onClick={onSave} disabled={!canSave}>
          Save
        </button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </Modal>
  );
}
