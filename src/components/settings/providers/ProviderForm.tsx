import { useEffect, useState } from 'react';
import { Modal } from '@/components/shared/Modal';
import type { ModelPickerEntry, ProviderProfile, ProviderType } from '@/lib/types';
import { TrustBadge } from '@/lib/provider_trust';
import { LockIcon } from '@/components/icons/LockIcon';
import { GlobeIcon } from '@/components/icons/GlobeIcon';
import { DEFAULT_URL } from '@/lib/provider_defaults';
import { ipc } from '@/lib/ipc';
import { ModelPicker } from './ModelPicker';
import {
  canSaveProvider,
  ctxLabel,
  detentsFor,
  isLocal,
  nearestCtxIndex,
  suggestContextLength,
  tierOf,
  usesModelPicker,
} from './providerUtils';
import { isAndroid } from '@/lib/platform';

/** Where an API key actually lives — the OS store differs per platform. */
const SECRET_STORE_NOTE = isAndroid()
  ? 'Encrypted with a key held in Android’s secure keystore, never stored in plain text.'
  : 'Stored in Windows Credential Manager, never on disk.';

export function ProviderForm({
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

  // Context-length auto-suggest (Round-6 Feature 1) — re-resolves whenever the
  // provider type or selected model changes; never applied automatically, only
  // offered (see the suggestion row below the slider).
  const [suggested, setSuggested] = useState<number | null>(null);
  const modelsKey = profile.models.join(',');
  useEffect(() => {
    let live = true;
    setSuggested(null);
    // Debounced: modelsKey changes on every keystroke in the Models field, and
    // suggestContextLength hits a live backend lookup — without this, typing
    // one model name fires a round-trip per character.
    const timer = setTimeout(() => {
      void suggestContextLength(profile).then((v) => live && setSuggested(v));
    }, 400);
    return () => {
      live = false;
      clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [profile.provider_type, profile.base_url, modelsKey]);
  const detents = detentsFor(suggested);
  const [advancedOpen, setAdvancedOpen] = useState(false);

  // Provider-add redesign: key-validate-then-pick-one-model flow for every
  // type except local/custom_openai/ollama, which keep the original
  // base-URL + free-text-models form untouched.
  const showModelPicker = usesModelPicker(profile.provider_type);
  const [modelOptions, setModelOptions] = useState<ModelPickerEntry[] | null>(null);
  const [validating, setValidating] = useState(false);
  const [validateError, setValidateError] = useState<string | null>(null);

  // Switching type or editing the key invalidates whatever list was already
  // loaded — it belonged to a different key/endpoint and would silently go
  // stale otherwise.
  useEffect(() => {
    setModelOptions(null);
    setValidateError(null);
  }, [profile.provider_type, secret]);

  // A blank secret is only checkable when editing an already-saved profile
  // (falls back to the keyring-backed variant below) — a brand-new profile
  // has nothing to validate yet.
  const canCheckKey = secret.trim().length > 0 || Boolean(profile.id);

  const checkKey = async () => {
    setValidating(true);
    setValidateError(null);
    try {
      const result =
        profile.id && !secret.trim()
          ? await ipc.discoverProviderModelsForSaved(profile.id)
          : await ipc.discoverProviderModels(profile.provider_type, profile.base_url, secret);
      setModelOptions(result);
    } catch (e) {
      setModelOptions(null);
      setValidateError(String(e));
    } finally {
      setValidating(false);
    }
  };

  // Save stays disabled until the picker has a selection, for the new-flow
  // types (see `canSaveProvider`'s own doc comment).
  const canSave = canSaveProvider(profile.models);

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
            set({ provider_type: pt, base_url: DEFAULT_URL[pt] });
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
      {/* Hidden for every model-picker-flow type (release-fixes-2: "no need
          to show the base URLs for OpenRouter or any of the new
          providers") — they're all fixed, well-known endpoints the user
          never needs to see or edit. Still shown for local/ollama/
          custom_openai, which genuinely need an editable endpoint. */}
      {!showModelPicker && (
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

      {showModelPicker ? (
        <>
          <label className="field">
            <span>API key {profile.id ? '(leave blank to keep)' : ''}</span>
            <input type="password" value={secret} onChange={(e) => onSecret(e.target.value)} />
            <small className="muted">{SECRET_STORE_NOTE}</small>
          </label>
          <div className="field">
            <span>Model</span>
            <div className="row">
              <button
                type="button"
                onClick={() => void checkKey()}
                disabled={!canCheckKey || validating}
              >
                {validating ? 'Checking…' : 'Check key & load models'}
              </button>
            </div>
            {validateError && (
              <small className="chat-error" role="alert">
                {validateError}
              </small>
            )}
            {modelOptions && (
              <ModelPicker
                models={modelOptions}
                value={profile.models[0] ?? null}
                onChange={(id) => set({ models: [id] })}
              />
            )}
            {!modelOptions && !validateError && !validating && (
              <small className="muted">Check your key to see available models.</small>
            )}
          </div>
        </>
      ) : (
        <label className="field">
          <span>Model</span>
          <input
            value={profile.models[0] ?? ''}
            onChange={(e) => set({ models: e.target.value.trim() ? [e.target.value.trim()] : [] })}
          />
          <small className="muted">
            One model per provider. To use another model here, duplicate this provider.
          </small>
        </label>
      )}

      {!showModelPicker && needsKey && (
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

      {/* Specialists run on whichever provider the daemon judges best — this is
          the one input to that decision that reflects what you want rather than
          what it can measure, so it sits with trust rather than under Advanced.
          A delegate is triage work: pointing it at a cheap, parallel endpoint is
          usually the right answer, and "never" keeps it off an expensive one
          entirely. */}
      <div className="field">
        <span>Use for specialists</span>
        <select
          value={profile.subagent_role ?? 'allowed'}
          onChange={(e) =>
            set({ subagent_role: e.target.value === 'allowed' ? null : e.target.value })
          }
        >
          <option value="allowed">If it is the best available</option>
          <option value="preferred">Prefer this one</option>
          <option value="never">Never</option>
        </select>
      </div>

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
            <span>Can this model use tools?</span>
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
          <label className="check">
            <input
              type="checkbox"
              checked={profile.supports_vision}
              onChange={(e) => set({ supports_vision: e.target.checked })}
            />
            <span>
              This provider&apos;s models accept images — override for vision models Kitty
              doesn&apos;t recognize by name (e.g. self-hosted or renamed).
            </span>
          </label>

          <label className="field">
            <span>Custom system prompt (optional — overrides Kitty&apos;s built-in default)</span>
            <textarea
              rows={4}
              value={profile.system_prompt ?? ''}
              placeholder="Default: Kitty's built-in system prompt…"
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
                checked={profile.top_p != null}
                onChange={(e) => set({ top_p: e.target.checked ? 0.8 : null })}
              />
              <span>Override top_p</span>
            </label>
            {profile.top_p != null && (
              <div className="row">
                <input
                  type="range"
                  min={0}
                  max={1}
                  step={0.05}
                  value={profile.top_p}
                  onChange={(e) => set({ top_p: Number(e.target.value) })}
                />
                <span className="status-badge">{profile.top_p.toFixed(2)}</span>
              </div>
            )}
          </div>

          <div className="field param-slider">
            <label className="check">
              <input
                type="checkbox"
                checked={profile.presence_penalty != null}
                onChange={(e) => set({ presence_penalty: e.target.checked ? 1.0 : null })}
              />
              <span>Override presence penalty</span>
            </label>
            {profile.presence_penalty != null && (
              <div className="row">
                <input
                  type="range"
                  min={0}
                  max={2}
                  step={0.1}
                  value={profile.presence_penalty}
                  onChange={(e) => set({ presence_penalty: Number(e.target.value) })}
                />
                <span className="status-badge">{profile.presence_penalty.toFixed(1)}</span>
              </div>
            )}
            <small className="muted">
              Repetition control, self-hosted providers only. Unset still applies a safe default —
              llama-server&rsquo;s own default allows endless loops — set this only to override it.
            </small>
          </div>

          {(profile.provider_type === 'ollama' || profile.provider_type === 'custom_openai') && (
            <div className="field param-slider">
              <label className="check">
                <input
                  type="checkbox"
                  checked={profile.top_k != null}
                  onChange={(e) => set({ top_k: e.target.checked ? 20 : null })}
                />
                <span>Override top_k</span>
              </label>
              {profile.top_k != null && (
                <div className="row">
                  <input
                    type="number"
                    min={0}
                    step={1}
                    value={profile.top_k}
                    onChange={(e) => set({ top_k: e.target.value ? Number(e.target.value) : null })}
                  />
                </div>
              )}
              <label className="check">
                <input
                  type="checkbox"
                  checked={profile.min_p != null}
                  onChange={(e) => set({ min_p: e.target.checked ? 0.0 : null })}
                />
                <span>Override min_p</span>
              </label>
              {profile.min_p != null && (
                <div className="row">
                  <input
                    type="number"
                    min={0}
                    max={1}
                    step={0.01}
                    value={profile.min_p}
                    onChange={(e) => set({ min_p: e.target.value ? Number(e.target.value) : null })}
                  />
                </div>
              )}
              <small className="muted">
                llama.cpp/Ollama-only sampling knobs — not part of the OpenAI or Anthropic API, so
                these are only ever sent to a self-hosted endpoint.
              </small>
            </div>
          )}

          <div className="field param-slider">
            <label className="check">
              <input
                type="checkbox"
                checked={profile.max_tokens != null}
                onChange={(e) => set({ max_tokens: e.target.checked ? 8192 : null })}
              />
              <span>Override max reply length (tokens)</span>
            </label>
            {profile.max_tokens != null && (
              <div className="row">
                <input
                  type="number"
                  min={1}
                  step={256}
                  value={profile.max_tokens}
                  onChange={(e) =>
                    set({ max_tokens: e.target.value ? Number(e.target.value) : null })
                  }
                />
              </div>
            )}
            <small className="muted">
              Hard cap on one reply. Self-hosted providers get a finite default (8192) even when
              this is unset, so no single reply can stream forever.
            </small>
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
            {profile.context_length != null && (
              <small className="muted">
                Used as this provider&apos;s context window, overriding the global token management
                value in Settings → Advanced.
              </small>
            )}
          </div>
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
