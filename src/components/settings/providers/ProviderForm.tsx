import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { Modal } from '@/components/shared/Modal';
import type { OllamaModel, ProviderProfile, ProviderType } from '@/lib/types';
import { TrustBadge } from '@/lib/provider_trust';
import { LockIcon } from '@/components/icons/LockIcon';
import { GlobeIcon } from '@/components/icons/GlobeIcon';
import { DEFAULT_URL } from '@/lib/provider_defaults';
import {
  ctxLabel,
  detentsFor,
  isLocal,
  LOCAL_NPU_PRESETS,
  nearestCtxIndex,
  suggestContextLength,
  tierOf,
} from './providerUtils';

export function ProviderForm({
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
