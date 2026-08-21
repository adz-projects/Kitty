import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { Modal } from '@/components/shared/Modal';
import type { OpenRouterCredits, ProviderProfile, ProviderView } from '@/lib/types';
import { TrustIcon, trustKind } from '@/lib/provider_trust';
import { ProviderTypeIcon, providerTypeLabel } from '@/components/icons/ProviderTypeIcon';
import { StarIcon } from '@/components/icons/StarIcon';
import { PencilIcon } from '@/components/icons/PencilIcon';
import { TrashIcon } from '@/components/icons/TrashIcon';
import { ProviderForm } from './providers/ProviderForm';
import { blank, hostOf, isLocal } from './providers/providerUtils';

export function Providers({ highlight }: { highlight: string | null }) {
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [editing, setEditing] = useState<ProviderProfile | null>(null);
  const [secret, setSecret] = useState('');
  const [confirmUntrusted, setConfirmUntrusted] = useState(false);
  const [handoffFor, setHandoffFor] = useState<ProviderView | null>(null);
  const [error, setError] = useState('');
  const [credits, setCredits] = useState<
    Record<
      string,
      | { status: 'loading' }
      | { status: 'error'; message: string }
      | { status: 'ok'; data: OpenRouterCredits }
    >
  >({});
  // Whether local inference is opted into at all (wizard redesign) — hides
  // the Ollama option from the type picker when the user picked the
  // API-key path and hasn't re-enabled it from Advanced.

  const refresh = () =>
    ipc
      .listProviders()
      .then(setProviders)
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
    setEditing(blank());
    setSecret('');
  };

  const doSave = async () => {
    if (!editing) return;
    try {
      await ipc.upsertProvider(editing, secret || null);
      setEditing(null);
      setConfirmUntrusted(false);
      setSecret('');
      await refresh();
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

  const activate = async (p: ProviderView, keepContext: boolean) => {
    if (!keepContext) {
      try {
        await ipc.setActiveSession({
          session_id: '',
          cwd: '',
          current_mode: 'auto',
          available_modes: [],
          thinking_effort: null,
          is_default_folder: true,
        });
      } catch {
        /* non-fatal */
      }
    }
    try {
      await ipc.activateProvider(p.id);
      setHandoffFor(null);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const onActivate = async (p: ProviderView) => {
    try {
      // Context-handoff gate: switching to an untrusted, non-local provider with an
      // active session forces an explicit keep/jettison choice (Round-2 item 18).
      if (!p.is_trusted && p.network_tier !== 'local') {
        const active = await ipc.getActiveSession();
        if (active && active.session_id) {
          setHandoffFor(p);
          return;
        }
      }
    } catch (e) {
      setError(String(e));
      return;
    }
    void activate(p, true);
  };

  const onDelete = async (p: ProviderView) => {
    if (!confirm(`Delete "${p.name}"?`)) return;
    try {
      await ipc.deleteProvider(p.id);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-section">
      <div className="row" style={{ justifyContent: 'space-between' }}>
        <h1>Providers</h1>
        <button className="primary" onClick={startNew}>
          Add provider
        </button>
      </div>
      {error && <div className="chat-error">{error}</div>}

      <div className="provider-list">
        {providers.length === 0 && (
          <p className="muted">
            No profiles yet. Add one, or use a local model from Settings &rarr; Local Models.
          </p>
        )}
        {providers.map((p) => (
          <div
            key={p.id}
            className={`provider-row${p.id === highlight ? ' highlight' : ''}${p.active ? ' active' : ''}`}
          >
            <div className="provider-row-info">
              <div className="provider-name">
                <span
                  className="provider-type-icon"
                  title={providerTypeLabel(p.provider_type)}
                  aria-label={providerTypeLabel(p.provider_type)}
                >
                  <ProviderTypeIcon type={p.provider_type} />
                </span>
                <span className="provider-name-text">{p.name || providerTypeLabel(p.provider_type)}</span>
                <span
                  title={`${trustKind(p.network_tier, p.is_trusted)} — ${hostOf(p.base_url)}`}
                  aria-label={trustKind(p.network_tier, p.is_trusted)}
                >
                  <TrustIcon tier={p.network_tier} isTrusted={p.is_trusted} />
                </span>
                {p.has_secret && <span title="API key stored">🔑</span>}
                {p.active && (
                  <span
                    title="Default for new chats — an already-open session keeps whatever provider it's already on, unaffected by this"
                    aria-label="Default for new sessions"
                  >
                    <StarIcon />
                  </span>
                )}
              </div>
              <div className="muted provider-url-line">{p.base_url}</div>
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
            {/* Icon-only, matching the app's black-and-white icon set. The row
                already carries a provider name, a type icon, a trust badge, a
                URL and sometimes a credit balance; three more words of button
                text was the thing pushing it wide. Each keeps its `title` (the
                full explanation on hover) and an `aria-label`, so nothing is
                lost for a screen reader or a first-time user. */}
            <div className="row provider-row-actions">
              {!p.active && (
                <button
                  className="icon-button"
                  onClick={() => void onActivate(p)}
                  title="Set as default — applies to brand-new chats, and doesn't change any chat you already have open"
                  aria-label={`Set ${p.name} as the default for new chats`}
                >
                  {/* Outline here, filled on the row above once it *is* the
                      default: the button and the marker are the same glyph at
                      two states of the same idea. */}
                  <StarIcon filled={false} />
                </button>
              )}
              <button
                className="icon-button"
                onClick={() => {
                  setEditing({ ...p });
                  setSecret('');
                }}
                title="Edit"
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
        ))}
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
        <Modal title="This provider isn’t marked trusted" onClose={() => setConfirmUntrusted(false)}>
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

      {handoffFor && (
        <Modal
          title="Send this conversation to an untrusted provider?"
          onClose={() => setHandoffFor(null)}
        >
          <p>
            The active session has context that would now be sent to{' '}
            <strong>{handoffFor.base_url}</strong>.
          </p>
          <div className="row">
            <button className="primary" onClick={() => void activate(handoffFor, true)}>
              Keep context (send it)
            </button>
            <button onClick={() => void activate(handoffFor, false)}>Start clean</button>
            <button onClick={() => setHandoffFor(null)}>Cancel</button>
          </div>
        </Modal>
      )}
    </section>
  );
}
