import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import type { LearningModelSetting, ProviderAccountView } from '@/lib/types';
import { modelLabel, modelTitle } from '@/lib/models';

type Kind = LearningModelSetting['kind'];

/** Whether two choices are the same (for the Save button). Exported for testing. */
export function sameLearningModel(a: LearningModelSetting, b: LearningModelSetting): boolean {
  if (a.kind !== b.kind) return false;
  if (a.kind === 'provider' && b.kind === 'provider') {
    return a.provider_id === b.provider_id && a.model === b.model;
  }
  return true;
}

/** Settings → Adaptive Pathway → Memory learning model: which model both
    memory engines learn with (extraction and cross-session synthesis).
    Mirrors `config::providers::learning`. */
export function LearningModel() {
  const [saved, setSaved] = useState<LearningModelSetting>({ kind: 'auto' });
  const [draft, setDraft] = useState<LearningModelSetting>({ kind: 'auto' });
  const [accounts, setAccounts] = useState<ProviderAccountView[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    void ipc
      .getMemoryLearningModel()
      .then((c) => {
        setSaved(c);
        setDraft(c);
      })
      .catch((e) => setError(String(e)));
    void ipc
      .listProviderAccounts()
      .then(setAccounts)
      .catch(() => {});
  }, []);

  const setKind = (kind: Kind) => {
    if (kind === 'provider') {
      setDraft({ kind, provider_id: '', model: '' });
    } else {
      setDraft({ kind });
    }
  };

  const save = async () => {
    setBusy(true);
    try {
      const next = await ipc.setMemoryLearningModel(draft);
      setSaved(next);
      setDraft(next);
      setError('');
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const account =
    draft.kind === 'provider' ? accounts.find((a) => a.id === draft.provider_id) : undefined;
  const incomplete = draft.kind === 'provider' && (!draft.provider_id || !draft.model);
  const dirty = !sameLearningModel(draft, saved);
  const helperLabel = isAndroid()
    ? "The chat's own model (no helper model on this device)"
    : "Helper model, else the chat's own model";

  return (
    <div className="mem-card">
      <div className="mem-card-head">
        <h2>Memory learning model</h2>
      </div>
      <p className="muted" style={{ margin: 0 }}>
        The model Adaptive Pathway and Memorabilia learn with: picking out beliefs and facts, and
        finding patterns across chats. Changes apply to the next pass.
      </p>
      {error && <div className="chat-error">{error}</div>}
      <div className="mem-setting-list">
        <label className="check">
          <input
            type="radio"
            name="learning-model"
            checked={draft.kind === 'auto'}
            disabled={busy}
            onChange={() => setKind('auto')}
          />
          <span>{helperLabel}</span>
        </label>
        <label className="check">
          <input
            type="radio"
            name="learning-model"
            checked={draft.kind === 'chat'}
            disabled={busy}
            onChange={() => setKind('chat')}
          />
          <span>The chat&apos;s own model (keeps this computer&apos;s CPU and GPU free)</span>
        </label>
        <label className="check">
          <input
            type="radio"
            name="learning-model"
            checked={draft.kind === 'provider'}
            disabled={busy}
            onChange={() => setKind('provider')}
          />
          <span>A specific model</span>
        </label>
        {draft.kind === 'provider' && (
          <div className="mem-suboptions row">
            <select
              value={draft.provider_id}
              disabled={busy}
              onChange={(e) =>
                setDraft({ kind: 'provider', provider_id: e.target.value, model: '' })
              }
            >
              <option value="">Provider…</option>
              {accounts.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name}
                </option>
              ))}
            </select>
            <select
              value={draft.model}
              disabled={busy || !account}
              onChange={(e) =>
                setDraft({
                  kind: 'provider',
                  provider_id: draft.provider_id,
                  model: e.target.value,
                })
              }
            >
              <option value="">Model…</option>
              {account?.model_infos.map((m) => (
                <option key={m.model} value={m.model} title={modelTitle(m)}>
                  {modelLabel(m)}
                </option>
              ))}
            </select>
          </div>
        )}
        <small className="muted">
          If the chosen model fails, learning falls back to the helper model or the chat&apos;s
          model rather than stopping.
        </small>
      </div>
      <div className="mem-card-actions">
        <button
          className="primary"
          disabled={busy || !dirty || incomplete}
          onClick={() => void save()}
        >
          Save
        </button>
      </div>
    </div>
  );
}
