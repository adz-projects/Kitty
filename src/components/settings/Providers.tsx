import { useEffect, useState } from 'react';
import { ipc, onEffortDetected } from '@/lib/ipc';
import { effortSummaryText } from '@/lib/effort';
import { Modal } from '@/components/shared/Modal';
import type {
  OpenRouterCredits,
  ProviderAccountView,
  ProviderProfile,
  ProviderView,
} from '@/lib/types';
import { TrustIcon, trustKind } from '@/lib/provider_trust';
import { cardLabel, groupByProvider, modelTitle } from '@/lib/models';
import { ProviderTypeIcon, providerTypeLabel } from '@/components/icons/ProviderTypeIcon';
import { KeyIcon } from '@/components/icons/KeyIcon';
import { PencilIcon } from '@/components/icons/PencilIcon';
import { TrashIcon } from '@/components/icons/TrashIcon';
import { ProviderForm } from './providers/ProviderForm';
import { blank, hostOf, isLocal } from './providers/providerUtils';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import { Banner } from '@/components/shared/Banner';

/** The stored provider inside an account view (the form edits only that). */
function profileOf(p: ProviderAccountView): ProviderProfile {
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  const { network_tier, has_secret, requires_key, model_infos, ...profile } = p;
  return profile;
}

/** Settings → Providers: the default model for new chats, and the providers
    whose checked models make up Kitty's model list. */
export function Providers({ highlight }: { highlight: string | null }) {
  const [accounts, setAccounts] = useState<ProviderAccountView[]>([]);
  const [cards, setCards] = useState<ProviderView[]>([]);
  const [editing, setEditing] = useState<ProviderProfile | null>(null);
  const [secret, setSecret] = useState('');
  const [confirmUntrusted, setConfirmUntrusted] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [needsDefault, setNeedsDefault] = useState(false);
  const [settingDefault, setSettingDefault] = useState(false);
  const [credits, setCredits] = useState<
    Record<
      string,
      | { status: 'loading' }
      | { status: 'error'; message: string }
      | { status: 'ok'; data: OpenRouterCredits }
    >
  >({});

  const refresh = () =>
    Promise.all([ipc.listProviderAccounts(), ipc.listProviders(), ipc.getConfig()])
      .then(([list, models, cfg]) => {
        setAccounts(list);
        setCards(models);
        setNeedsDefault(cfg.needs_default_provider);
      })
      .catch((e) => setError(String(e)));
  useEffect(() => void refresh(), []);
  // A model's reasoning detection finished — after a save, a Re-detect, or in
  // the background at startup — so the default's line is current.
  useEffect(() => {
    const unlisten = onEffortDetected(() => void refresh());
    return () => void unlisten.then((u) => u());
  }, []);
  const [redetecting, setRedetecting] = useState<Set<string>>(new Set());
  const redetect = async (id: string | null) => {
    const key = id ?? '*';
    setRedetecting((prev) => new Set(prev).add(key));
    await refresh();
    try {
      await (id ? ipc.redetectEffort(id) : ipc.redetectEffortAll());
    } catch (e) {
      setError(String(e));
    } finally {
      setRedetecting((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
      await refresh();
    }
  };
  const checkCredits = async (id: string) => {
    setCredits((prev) => ({ ...prev, [id]: { status: 'loading' } }));
    try {
      const data = await ipc.openrouterCredits(id);
      setCredits((prev) => ({ ...prev, [id]: { status: 'ok', data } }));
    } catch (e) {
      setCredits((prev) => ({ ...prev, [id]: { status: 'error', message: String(e) } }));
    }
  };

  const startNew = () => {
    setEditing({ ...blank(), name: 'OpenRouter' });
    setSecret('');
  };

  const doSave = async () => {
    if (!editing) return;
    try {
      const hadDefault = cards.some((c) => c.active);
      await ipc.upsertProvider(editing, secret || null);
      setEditing(null);
      setConfirmUntrusted(false);
      setSecret('');
      await refresh();
      if (!hadDefault) {
        const now = await ipc.listProviders();
        const def = now.find((c) => c.active);
        if (def) setNotice(`${cardLabel(def)} is now the default model for new chats.`);
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const onSave = () => {
    if (!editing) return;
    // Saving a non-local provider the user hasn't marked trusted requires an
    // explicit acknowledgement (Round-2 item 18 — was tier===remote).
    if (!editing.is_trusted && !isLocal(editing.base_url)) setConfirmUntrusted(true);
    else void doSave();
  };

  // The default for new chats only; open chats keep their own model
  // (decision #50). The model must pass a connection check first.
  const onDefault = async (cardId: string) => {
    if (!cardId) return;
    setSettingDefault(true);
    setError('');
    try {
      await ipc.setDefaultProvider(cardId);
      await refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setSettingDefault(false);
    }
  };

  const onDelete = async (p: ProviderAccountView) => {
    const ok = await confirmDialog({
      title: `Delete "${p.name}"?`,
      message:
        'Its saved API key is deleted too, and its models leave the model list. Chats that used them stay readable but cannot continue on them.',
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
    const hadDefault = cards.find((c) => c.active)?.info.provider_id === p.id;
    try {
      const next = await ipc.deleteProvider(p.id);
      await refresh();
      if (hadDefault) {
        const now = await ipc.listProviders();
        const def = now.find((c) => c.id === next);
        setNotice(
          def
            ? `${cardLabel(def, true)} is now the default model for new chats.`
            : 'There is no default model now. Add a provider to start new chats.'
        );
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const active = cards.find((c) => c.active) ?? null;
  const usable = cards.filter((c) => !c.disabled_reason);
  const highlighted = (p: ProviderAccountView) =>
    !!highlight && (p.id === highlight || Object.values(p.model_ids).includes(highlight));

  return (
    <section className="settings-section">
      <div className="row" style={{ justifyContent: 'space-between' }}>
        <h1>Providers</h1>
        <div className="row">
          <button
            onClick={() => void redetect(null)}
            disabled={redetecting.has('*') || cards.length === 0}
            title="Check again which reasoning levels each model offers"
          >
            {redetecting.has('*') ? 'Re-detecting…' : 'Re-detect reasoning'}
          </button>
          <button className="primary" onClick={startNew}>
            Add provider
          </button>
        </div>
      </div>
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
          Your default model ran on this device, which Kitty no longer supports. Choose another
          default model below.
        </Banner>
      )}

      <label className="field default-model-field">
        <span>Default model for new chats</span>
        <select
          value={active?.id ?? ''}
          disabled={settingDefault || usable.length === 0}
          onChange={(e) => void onDefault(e.target.value)}
          title={active ? modelTitle(active.info) : undefined}
        >
          {!active && (
            <option value="" disabled>
              {usable.length === 0 ? 'Add a provider first' : 'Choose a model'}
            </option>
          )}
          {groupByProvider(usable).map((g) => (
            <optgroup key={g.providerId} label={g.provider}>
              {g.cards.map((c) => (
                <option key={c.id} value={c.id}>
                  {cardLabel(c)}
                </option>
              ))}
            </optgroup>
          ))}
        </select>
        <small className="muted">
          {settingDefault
            ? 'Checking the model…'
            : 'Chats already open keep their own model. Models are ordered by intelligence and how recent each is; $ is up to $3 per million output tokens, $$ up to $8, $$$ more.'}
        </small>
        {active && (
          <small className="muted provider-effort-line">
            {(() => {
              const busy =
                active.effort.detecting || redetecting.has(active.id) || redetecting.has('*');
              return (
                <>
                  {busy && active.effort.detected_at !== null
                    ? 'Detecting reasoning levels…'
                    : effortSummaryText(active.effort, Math.floor(Date.now() / 1000))}{' '}
                  <button
                    className="link"
                    disabled={busy}
                    onClick={() => void redetect(active.id)}
                    title="Check again which reasoning levels this model offers. The current levels stay if the check fails."
                  >
                    Re-detect
                  </button>
                </>
              );
            })()}
          </small>
        )}
      </label>

      <div className="provider-list">
        {accounts.length === 0 && (
          <p className="muted">No providers yet. Add one to start chatting.</p>
        )}
        {accounts.map((p) => {
          const checked = p.model_infos.filter((m) => m.enabled).length;
          return (
            <div key={p.id} className={`provider-row${highlighted(p) ? ' highlight' : ''}`}>
              <div className="provider-row-info">
                <div className="provider-name">
                  <span
                    className="provider-type-icon"
                    title={providerTypeLabel(p.provider_type)}
                    aria-label={providerTypeLabel(p.provider_type)}
                  >
                    <ProviderTypeIcon type={p.provider_type} />
                  </span>
                  <span className="provider-name-text">
                    {p.name || providerTypeLabel(p.provider_type)}
                  </span>
                  <span
                    title={`${trustKind(p.network_tier, p.is_trusted)} — ${hostOf(p.base_url)}`}
                    aria-label={trustKind(p.network_tier, p.is_trusted)}
                  >
                    <TrustIcon tier={p.network_tier} isTrusted={p.is_trusted} />
                  </span>
                  {p.has_secret && (
                    <span title="API key stored" aria-label="API key stored">
                      <KeyIcon />
                    </span>
                  )}
                </div>
                <div className="muted provider-url-line">
                  {checked} of {p.models.length} model{p.models.length === 1 ? '' : 's'} checked
                  {['custom_openai', 'ollama', 'local'].includes(p.provider_type) &&
                    ` · ${p.base_url}`}
                </div>
                {p.disabled_reason && (
                  <div className="muted">This provider type is no longer supported.</div>
                )}
                {p.provider_type === 'openrouter' && p.has_secret && (
                  <div className="muted" style={{ fontSize: 14 }}>
                    {!credits[p.id] && (
                      <button className="link" onClick={() => void checkCredits(p.id)}>
                        Check credits
                      </button>
                    )}
                    {credits[p.id]?.status === 'loading' && 'Checking…'}
                    {credits[p.id]?.status === 'error' && (
                      <>
                        {(credits[p.id] as { status: 'error'; message: string }).message}{' '}
                        <button className="link" onClick={() => void checkCredits(p.id)}>
                          Retry
                        </button>
                      </>
                    )}
                    {credits[p.id]?.status === 'ok' &&
                      (() => {
                        const d = (credits[p.id] as { status: 'ok'; data: OpenRouterCredits }).data;
                        const remaining =
                          d.limit_remaining != null
                            ? `$${d.limit_remaining.toFixed(2)} remaining`
                            : d.is_free_tier
                              ? 'Free tier'
                              : 'No spend limit set';
                        return (
                          <>
                            {remaining} · ${d.usage.toFixed(2)} used
                            {d.limit != null ? ` of $${d.limit.toFixed(2)}` : ''}{' '}
                            <button className="link" onClick={() => void checkCredits(p.id)}>
                              Refresh
                            </button>
                          </>
                        );
                      })()}
                  </div>
                )}
              </div>
              <div className="row provider-row-actions">
                <button
                  className="icon-button"
                  onClick={() => {
                    setEditing(profileOf(p));
                    setSecret('');
                  }}
                  title="Edit — key, settings, and which models are checked"
                  aria-label={`Edit ${p.name}`}
                >
                  <PencilIcon />
                </button>
                <button
                  className="icon-button"
                  onClick={() => void onDelete(p)}
                  title="Delete"
                  aria-label={`Delete ${p.name}`}
                >
                  <TrashIcon />
                </button>
              </div>
            </div>
          );
        })}
      </div>
      {editing && (
        <ProviderForm
          profile={editing}
          secret={secret}
          onChange={setEditing}
          onSecret={setSecret}
          onCancel={() => setEditing(null)}
          onSave={onSave}
        />
      )}

      {confirmUntrusted && editing && (
        <Modal
          title="This provider isn’t marked trusted"
          onClose={() => setConfirmUntrusted(false)}
        >
          <p>
            Prompts, file contents, and tool outputs may be transmitted to{' '}
            <strong>{hostOf(editing.base_url)}</strong> — an unverified third party. Mark it trusted
            in the form if you control it.
          </p>
          <div className="row">
            <button className="primary" onClick={() => void doSave()}>
              I understand — save anyway
            </button>
            <button onClick={() => setConfirmUntrusted(false)}>Cancel</button>
          </div>
        </Modal>
      )}
    </section>
  );
}
