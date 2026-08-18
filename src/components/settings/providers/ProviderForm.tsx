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
  // Neither the in-process engine nor an Ollama server takes an API key —
  // for Ollama that's a property of the server, and stays true now that the
  // user runs it rather than Kitty.
  const needsKey = profile.provider_type !== 'ollama' && profile.provider_type !== 'local';

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
  const canSave = canSaveProvider(profile.provider_type, profile.models);

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
          <option value="local">On this device</option>
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
              This is a Tailscale address, so one URL works both at home and away: BigTiny
              automatically tries a direct LAN connection first when you&rsquo;re on the same
              network as the server, and falls back to routing over Tailscale otherwise — no need
              to switch URLs manually.
            </small>
          )}
        </label>
      )}

      {showModelPicker ? (
        <>
          <label className="field">
            <span>API key {profile.id ? '(leave blank to keep)' : ''}</span>
            <input type="password" value={secret} onChange={(e) => onSecret(e.target.value)} />
            <small className="muted">Stored in Windows Credential Manager, never on disk.</small>
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

      {!showModelPicker && needsKey && (
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
          <label className="check">
            <input
              type="checkbox"
              checked={profile.strip_reasoning}
              onChange={(e) => set({ strip_reasoning: e.target.checked })}
            />
            <span>
              Strip reasoning from context on later turns (recommended for Gemma-style local
              reasoning models, chat-only providers only)
            </span>
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
                Used as BigTiny&apos;s max_context_tokens for this provider, overriding the global
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
