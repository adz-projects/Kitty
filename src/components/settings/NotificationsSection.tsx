import { useConfigDraft } from './useConfigDraft';
import type { NotificationPrefs } from '@/lib/types';

const EVENTS: { key: keyof NotificationPrefs; label: string }[] = [
  { key: 'approval_needed', label: 'A tool call needs approval' },
  { key: 'task_complete', label: 'A task completed' },
  { key: 'task_failed', label: 'A task failed' },
  { key: 'stack_degraded', label: 'The stack became degraded' },
];

/** Per-event notification toggles (fired only when the overlay is hidden). */
export function NotificationsSection() {
  const { draft, update, save, saved, error } = useConfigDraft();
  if (!draft) return <p className="muted">Loading…</p>;
  // A loaded config predating the notifications field (or hand-edited to
  // drop it) would otherwise crash this section with "cannot read property
  // of undefined" — default the object instead.
  const notifications = draft.notifications ?? {};

  return (
    <section className="settings-section">
      <h1>Notifications</h1>
      <p className="muted">Native notifications fire only when the overlay is hidden.</p>
      {EVENTS.map((e) => (
        <label className="check" key={e.key}>
          <input
            type="checkbox"
            checked={notifications[e.key] ?? false}
            onChange={(ev) =>
              update({ notifications: { ...notifications, [e.key]: ev.target.checked } })
            }
          />
          <span>{e.label}</span>
        </label>
      ))}
      <div className="row">
        <button className="primary" onClick={() => void save()}>
          Save
        </button>
        {saved && <span className="muted">Saved.</span>}
        {error && <span className="error">Couldn't save: {error}</span>}
      </div>
    </section>
  );
}
