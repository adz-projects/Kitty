import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { Modal } from '@/components/shared/Modal';
import type { OpenRouterCredits, ProviderProfile, ProviderView } from '@/lib/types';
import { TrustBadge } from '@/lib/provider_trust';
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
            <div>
              <div className="provider-name">
                {p.name || p.provider_type}{' '}
                <span className="status-badge">
                  <TrustBadge tier={p.network_tier} isTrusted={p.is_trusted} />
                </span>
                {p.active && (
                  <span
                    className="status-badge"
                    title="Used for brand-new chats — an already-open session keeps whatever provider it's already on, unaffected by this"
                  >
                    default for new sessions
                  </span>
                )}
              </div>
              <div className="muted" style={{ fontSize: 14 }}>
                {p.provider_type} · {p.base_url}
                {p.has_secret ? ' · 🔑 key stored' : ''}
              </div>
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
            <div className="row">
              {!p.active && (
                <button
                  onClick={() => void onActivate(p)}
                  title="Sets the default for brand-new chats — doesn't change any chat you already have open"
                >
                  Set as default
                </button>
              )}
              <button
                onClick={() => {
                  setEditing({ ...p });
                  setSecret('');
                }}
              >
                Edit
              </button>
              <button onClick={() => void onDelete(p)}>Delete</button>
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
