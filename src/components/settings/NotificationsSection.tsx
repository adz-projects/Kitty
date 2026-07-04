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
  const { draft, update, save, saved } = useConfigDraft();
  if (!draft) return <p className="muted">Loading…</p>;

  return (
    <section className="settings-section">
      <h1>Notifications</h1>
      <p className="muted">Native notifications fire only when the overlay is hidden.</p>
      {EVENTS.map((e) => (
        <label className="check" key={e.key}>
          <input
            type="checkbox"
            checked={draft.notifications[e.key]}
            onChange={(ev) =>
              update({ notifications: { ...draft.notifications, [e.key]: ev.target.checked } })
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
      </div>
    </section>
  );
}
