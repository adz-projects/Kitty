import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { Modal } from '@/components/shared/Modal';
import type {
  OpenRouterCredits,
  ProviderAccountView,
  ProviderProfile,
  ProviderView,
} from '@/lib/types';
import { TrustIcon, trustKind } from '@/lib/provider_trust';
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

/** Settings → Providers: the services Kitty talks to, each with its key and
    the models checked for use in setups (Settings → Setups). */
export function Providers({ highlight }: { highlight: string | null }) {
  const [accounts, setAccounts] = useState<ProviderAccountView[]>([]);
  const [cards, setCards] = useState<ProviderView[]>([]);
  const [editing, setEditing] = useState<ProviderProfile | null>(null);
  const [secret, setSecret] = useState('');
  const [confirmUntrusted, setConfirmUntrusted] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [credits, setCredits] = useState<
    Record<
      string,
      | { status: 'loading' }
      | { status: 'error'; message: string }
      | { status: 'ok'; data: OpenRouterCredits }
    >
  >({});

  const refresh = () =>
    Promise.all([ipc.listProviderAccounts(), ipc.listProviders()])
      .then(([list, setups]) => {
        setAccounts(list);
        setCards(setups);
      })
      .catch((e) => setError(String(e)));
  useEffect(() => void refresh(), []);
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
        if (def) {
          setNotice(
            `Kitty added ${def.info.name} under Models and chose it for new chats. Add more there.`
          );
        }
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

  const onDelete = async (p: ProviderAccountView) => {
    const ok = await confirmDialog({
      title: `Delete "${p.name}"?`,
      message:
        'Its saved API key, and every model of it under Models, are deleted too. Chats that used them stay readable, and continue once you choose another model.',
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
            ? `${def.name} is now the default model for new chats.`
            : 'There is no default model now. Add a provider to start new chats.'
        );
      }
    } catch (e) {
      setError(String(e));
    }
  };

  // A deep link names a provider, or a setup (a chat's error card).
  const highlighted = (p: ProviderAccountView) =>
    !!highlight &&
    (p.id === highlight || cards.some((c) => c.id === highlight && c.info.provider_id === p.id));

  return (
    <section className="settings-section">
      <div className="row" style={{ justifyContent: 'space-between' }}>
        <h1>Providers</h1>
        <div className="row">
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
                  {checked} of {p.models.length} model{p.models.length === 1 ? '' : 's'} checked ·{' '}
                  {(() => {
                    const n = cards.filter((c) => c.info.provider_id === p.id).length;
                    return `${n} in use under Models`;
                  })()}
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
          setups={cards.map((c) => c.setup)}
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
