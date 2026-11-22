import { useEffect, useMemo, useState } from 'react';
import { Modal } from '@/components/shared/Modal';
import { ipc } from '@/lib/ipc';
import { modelLabel, modelTitle } from '@/lib/models';
import { isAndroid } from '@/lib/platform';
import type { ModelInfo, ProviderAccountView, Setup, ThinkingEffort } from '@/lib/types';
import { ctxLabel, detentsFor, nearestCtxIndex } from '../providers/providerUtils';
import { checkedModels, modelKey, splitModelKey } from './setupUtils';

/** Add or edit a setup: a checked model of a provider, with a name, a
    default thinking level, an optional context length, an optional system
    prompt and whether specialists may run on it. */
export function SetupForm({
  setup,
  accounts,
  onChange,
  onCancel,
  onSave,
  saving,
}: {
  setup: Setup;
  accounts: ProviderAccountView[];
  onChange: (s: Setup) => void;
  onCancel: () => void;
  onSave: () => void;
  saving: boolean;
}) {
  const set = (patch: Partial<Setup>) => onChange({ ...setup, ...patch });
  const groups = useMemo(() => checkedModels(accounts), [accounts]);
  const info: ModelInfo | null = useMemo(
    () =>
      groups
        .flatMap((g) => g.models)
        .find((m) => m.provider_id === setup.provider_id && m.model === setup.model) ?? null,
    [groups, setup.provider_id, setup.model]
  );
  const provider = accounts.find((a) => a.id === setup.provider_id) ?? null;
  const unavailable = !!setup.model && !info;

  // The model's thinking levels, detected on demand.
  const [effort, setEffort] = useState<ThinkingEffort | null>(null);
  const [effortLoading, setEffortLoading] = useState(false);
  useEffect(() => {
    let live = true;
    setEffort(null);
    if (!setup.provider_id || !setup.model) return;
    setEffortLoading(true);
    void ipc
      .getModelEffort(setup.provider_id, setup.model)
      .then((e) => live && setEffort(e))
      .catch(() => live && setEffort(null))
      .finally(() => live && setEffortLoading(false));
    return () => {
      live = false;
    };
  }, [setup.provider_id, setup.model]);

  const pickModel = (key: string) => {
    const { providerId, model } = splitModelKey(key);
    const next = groups
      .flatMap((g) => g.models)
      .find((m) => m.provider_id === providerId && m.model === model);
    // A name the user has not changed follows the model.
    const followName = !setup.name.trim() || setup.name === info?.name;
    set({
      provider_id: providerId,
      model,
      name: followName ? (next?.name ?? model) : setup.name,
      // Another model has another ladder (and another window).
      effort: null,
      context_length: null,
    });
  };

  const detected = info?.context_length ?? provider?.context_lengths[setup.model] ?? null;
  const detents = detentsFor(detected);
  const canSave = !!setup.provider_id && !!setup.model && !unavailable && !saving;

  return (
    <Modal title={setup.id ? 'Edit model' : 'Add model'} onClose={onCancel}>
      <label className="field">
        <span>Model</span>
        <select
          value={setup.model ? modelKey(setup.provider_id, setup.model) : ''}
          onChange={(e) => pickModel(e.target.value)}
          title={info ? modelTitle(info) : undefined}
        >
          {!setup.model && (
            <option value="" disabled>
              Choose a model
            </option>
          )}
          {unavailable && (
            <option value={modelKey(setup.provider_id, setup.model)} disabled>
              {setup.model} (no longer checked)
            </option>
          )}
          {groups.map((g) => (
            <optgroup key={g.providerId} label={g.provider}>
              {g.models.map((m) => (
                <option key={m.model} value={modelKey(g.providerId, m.model)} title={modelTitle(m)}>
                  {modelLabel(m)}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
        <small className="muted">
          {unavailable
            ? 'This model is no longer checked in its provider. Choose another, or check it again under Providers.'
            : 'The models checked under Providers, best first. $ is up to $3 per million output tokens, $$ up to $8, $$$ more.'}
          {setup.id && !unavailable && ' Chats already using it keep the model they started on.'}
        </small>
      </label>

      <label className="field">
        <span>Name</span>
        <input
          value={setup.name}
          placeholder={info?.name ?? 'A name for the model menu'}
          onChange={(e) => set({ name: e.target.value })}
        />
      </label>

      <label className="field">
        <span>Thinking level</span>
        <select
          value={setup.effort ?? ''}
          disabled={!effort}
          onChange={(e) => set({ effort: e.target.value || null })}
        >
          <option value="">
            {effort
              ? `Model default (${effort.options.find((o) => o.value === effort.current_value)?.name ?? effort.current_value})`
              : 'Model default'}
          </option>
          {effort?.options.map((o) => (
            <option key={o.value} value={o.value}>
              {o.name}
            </option>
          ))}
        </select>
        <small className="muted">
          {effortLoading
            ? 'Checking which levels this model offers…'
            : !setup.model
              ? 'Choose a model first.'
              : effort
                ? 'Where a new chat starts. Change it turn by turn beside the model menu.'
                : 'Kitty found no thinking levels it can set on this model.'}
        </small>
      </label>

      <div className="field param-slider">
        <label className="check">
          <input
            type="checkbox"
            checked={setup.context_length != null}
            onChange={(e) => set({ context_length: e.target.checked ? (detected ?? 32768) : null })}
          />
          <span>Set the context length</span>
        </label>
        {setup.context_length != null ? (
          <div className="row">
            <input
              type="range"
              min={0}
              max={detents.length - 1}
              step={1}
              value={nearestCtxIndex(detents, setup.context_length)}
              onChange={(e) => set({ context_length: detents[Number(e.target.value)] })}
            />
            <span className="status-badge">{ctxLabel(setup.context_length)}</span>
          </div>
        ) : null}
        <small className="muted">
          {detected
            ? `Detected for this model: ${ctxLabel(detected)} tokens.`
            : 'Kitty detects the window when the model is first used.'}{' '}
          Set a smaller one to keep long chats cheaper.
        </small>
      </div>

      <label className="field">
        <span>System prompt</span>
        <textarea
          rows={6}
          value={setup.system_prompt ?? ''}
          placeholder="Kitty's built-in prompt"
          onChange={(e) => set({ system_prompt: e.target.value || null })}
        />
        <small className="muted">
          Leave empty for Kitty&apos;s built-in prompt. A change reaches new chats; a chat keeps the
          prompt it started with, so its prompt cache keeps working.
        </small>
      </label>

      {!isAndroid() && (
        <label className="field">
          <span>Use for specialists</span>
          <select
            value={setup.subagent_role ?? 'allowed'}
            onChange={(e) =>
              set({ subagent_role: e.target.value === 'allowed' ? null : e.target.value })
            }
          >
            <option value="allowed">If it is the best available</option>
            <option value="preferred">Prefer this model</option>
            <option value="never">Never</option>
          </select>
          <small className="muted">
            Whether specialists may run on it. A cheap, fast model is usually the one to prefer.
          </small>
        </label>
      )}

      <div className="row">
        <button className="primary" onClick={onSave} disabled={!canSave}>
          {saving ? 'Saving…' : 'Save'}
        </button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </Modal>
  );
}
