import { useEffect, useState } from 'react';
import { ipc, onEffortDetected } from '@/lib/ipc';
import { effortSummaryText } from '@/lib/effort';
import { priceHint } from '@/lib/models';
import type { ProviderAccountView, ProviderView, Setup } from '@/lib/types';
import { CopyIcon } from '@/components/icons/CopyIcon';
import { PencilIcon } from '@/components/icons/PencilIcon';
import { TrashIcon } from '@/components/icons/TrashIcon';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import { Banner } from '@/components/shared/Banner';
import { SetupForm } from './setups/SetupForm';
import { blankSetup, checkedModels, duplicateOf, moved, setupTitle } from './setups/setupUtils';

/** Settings → Setups: what chats run on. Each is a checked model with a
    name, a thinking level, a context length and a system prompt; the chat's
    model menu lists them in this order. */
export function Setups({ highlight }: { highlight: string | null }) {
  const [cards, setCards] = useState<ProviderView[]>([]);
  const [accounts, setAccounts] = useState<ProviderAccountView[]>([]);
  const [editing, setEditing] = useState<Setup | null>(null);
  const [saving, setSaving] = useState(false);
  const [settingDefault, setSettingDefault] = useState(false);
  const [needsDefault, setNeedsDefault] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [redetecting, setRedetecting] = useState(false);

  const refresh = () =>
    Promise.all([ipc.listProviders(), ipc.listProviderAccounts(), ipc.getConfig()])
      .then(([c, a, cfg]) => {
        setCards(c);
        setAccounts(a);
        setNeedsDefault(cfg.needs_default_provider);
      })
      .catch((e) => setError(String(e)));
  useEffect(() => void refresh(), []);
  useEffect(() => {
    const unlisten = onEffortDetected(() => void refresh());
    return () => void unlisten.then((u) => u());
  }, []);

  const hasModels = checkedModels(accounts).length > 0;
  const active = cards.find((c) => c.active) ?? null;
  const usable = cards.filter((c) => !c.disabled_reason);

  const save = async () => {
    if (!editing) return;
    setSaving(true);
    setError('');
    try {
      await ipc.upsertSetup(editing);
      setEditing(null);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const onDefault = async (id: string) => {
    if (!id) return;
    setSettingDefault(true);
    setError('');
    try {
      await ipc.setDefaultProvider(id);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setSettingDefault(false);
    }
  };

  const onDelete = async (c: ProviderView) => {
    const ok = await confirmDialog({
      title: `Delete "${c.name}"?`,
      message: 'Chats that use it stay readable, but continue only after you choose another setup.',
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
    try {
      const next = await ipc.deleteSetup(c.id);
      await refresh();
      if (c.active) {
        const now = await ipc.listProviders();
        const def = now.find((x) => x.id === next);
        setNotice(
          def
            ? `${def.name} is now the default setup for new chats.`
            : 'There is no default setup now. Add one to start new chats.'
        );
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const move = async (index: number, delta: number) => {
    const order = moved(cards, index, delta);
    if (order === cards) return;
    setCards(order);
    try {
      await ipc.reorderSetups(order.map((c) => c.id));
    } catch (e) {
      setError(String(e));
    }
    await refresh();
  };

  const redetectAll = async () => {
    setRedetecting(true);
    try {
      await ipc.redetectEffortAll();
    } catch (e) {
      setError(String(e));
    } finally {
      setRedetecting(false);
      await refresh();
    }
  };

  const now = Math.floor(Date.now() / 1000);

  return (
    <section className="settings-section">
      <div className="row" style={{ justifyContent: 'space-between' }}>
        <h1>Setups</h1>
        <div className="row">
          <button
            onClick={() => void redetectAll()}
            disabled={redetecting || cards.length === 0}
            title="Check again which thinking levels each model offers"
          >
            {redetecting ? 'Re-detecting…' : 'Re-detect thinking levels'}
          </button>
          <button
            className="primary"
            onClick={() => setEditing(blankSetup(accounts))}
            disabled={!hasModels}
            title={hasModels ? undefined : 'Add a provider and check a model first'}
          >
            Add setup
          </button>
        </div>
      </div>
      <p className="muted">
        A setup is a model with its own name, thinking level, context length and system prompt.
        Chats, scheduled tasks and specialists run on setups.
      </p>
      {error && <div className="chat-error">{error}</div>}
      {notice && (
        <Banner
          tone="ok"
          actions={
            <button className="link" onClick={() => setNotice('')}>
              Dismiss
            </button>
          }
        >
          {notice}
        </Banner>
      )}
      {needsDefault && (
        <Banner tone="warn">
          Your default ran on this device, which Kitty no longer supports. Choose another default
          setup below.
        </Banner>
      )}

      <label className="field default-model-field">
        <span>Default setup for new chats</span>
        <select
          value={active?.id ?? ''}
          disabled={settingDefault || usable.length === 0}
          onChange={(e) => void onDefault(e.target.value)}
          title={active ? setupTitle(active) : undefined}
        >
          {!active && (
            <option value="" disabled>
              {usable.length === 0 ? 'Add a setup first' : 'Choose a setup'}
            </option>
          )}
          {usable.map((c) => (
            <option key={c.id} value={c.id} title={setupTitle(c)}>
              {c.name}
            </option>
          ))}
        </select>
        <small className="muted">
          {settingDefault ? 'Checking the model…' : 'Chats already open keep their own setup.'}
        </small>
      </label>

      <div className="provider-list">
        {cards.length === 0 && (
          <p className="muted">
            {hasModels
              ? 'No setups yet. Add one to start chatting.'
              : 'No setups yet. Add a provider and check its models first.'}
          </p>
        )}
        {cards.map((c, i) => {
          const price = priceHint(c.info);
          const level = c.setup.effort;
          return (
            <div key={c.id} className={`provider-row${highlight === c.id ? ' highlight' : ''}`}>
              <div className="provider-row-info">
                <div className="provider-name">
                  <span className="provider-name-text">{c.name}</span>
                  {c.active && <span className="status-badge">Default</span>}
                </div>
                <div className="muted provider-url-line" title={price ?? undefined}>
                  {setupTitle(c)}
                  {c.info.price_tier && ` · ${c.info.price_tier}`}
                </div>
                <div className="muted provider-url-line">
                  {[
                    level ? `Thinking: ${level}` : null,
                    c.setup.context_length
                      ? `Context: ${c.setup.context_length.toLocaleString()} tokens`
                      : null,
                    c.setup.system_prompt ? 'Own system prompt' : null,
                  ]
                    .filter(Boolean)
                    .join(' · ') || effortSummaryText(c.effort, now)}
                </div>
                {c.disabled_reason === 'model_unavailable' && (
                  <div className="chat-error">
                    Its model is no longer checked under Providers. Edit the setup to choose
                    another.
                  </div>
                )}
                {c.disabled_reason === 'unsupported' && (
                  <div className="muted">Its provider type is no longer supported.</div>
                )}
              </div>
              <div className="row provider-row-actions">
                <button
                  className="icon-button"
                  onClick={() => void move(i, -1)}
                  disabled={i === 0}
                  title="Move up"
                  aria-label={`Move ${c.name} up`}
                >
                  ↑
                </button>
                <button
                  className="icon-button"
                  onClick={() => void move(i, 1)}
                  disabled={i === cards.length - 1}
                  title="Move down"
                  aria-label={`Move ${c.name} down`}
                >
                  ↓
                </button>
                <button
                  className="icon-button"
                  onClick={() => setEditing({ ...c.setup })}
                  title="Edit"
                  aria-label={`Edit ${c.name}`}
                >
                  <PencilIcon />
                </button>
                <button
                  className="icon-button"
                  onClick={() => setEditing(duplicateOf(c.setup))}
                  title="Duplicate"
                  aria-label={`Duplicate ${c.name}`}
                >
                  <CopyIcon />
                </button>
                <button
                  className="icon-button"
                  onClick={() => void onDelete(c)}
                  title="Delete"
                  aria-label={`Delete ${c.name}`}
                >
                  <TrashIcon />
                </button>
              </div>
            </div>
          );
        })}
      </div>

      {editing && (
        <SetupForm
          setup={editing}
          accounts={accounts}
          onChange={setEditing}
          onCancel={() => setEditing(null)}
          onSave={() => void save()}
          saving={saving}
        />
      )}
    </section>
  );
}
