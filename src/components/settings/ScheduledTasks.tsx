import { useEffect, useState } from 'react';
import { ipc, onScheduledTasksChanged, pickFolder } from '@/lib/ipc';
import type { LocalModel, Schedule, ScheduledTask } from '@/lib/types';
import { Modal } from '@/components/shared/Modal';

export type IntervalUnit = 'minutes' | 'hours' | 'days';
export const UNIT_SECONDS: Record<IntervalUnit, number> = {
  minutes: 60,
  hours: 3600,
  days: 86400,
};

/** Reverse-maps `interval_secs` to the largest whole unit that divides it
    evenly, for displaying an existing recurring task's interval in the form. */
export function secondsToAmountUnit(secs: number): { amount: number; unit: IntervalUnit } {
  if (secs % UNIT_SECONDS.days === 0) return { amount: secs / UNIT_SECONDS.days, unit: 'days' };
  if (secs % UNIT_SECONDS.hours === 0) return { amount: secs / UNIT_SECONDS.hours, unit: 'hours' };
  return { amount: Math.max(1, Math.round(secs / UNIT_SECONDS.minutes)), unit: 'minutes' };
}

/** `datetime-local`'s value has no timezone — it's always local wall-clock
    time, which is exactly what `next_fire` (a `DateTime<Local>` on the Rust
    side) needs. */
function toDatetimeLocalValue(iso: string): string {
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function scheduleSummary(task: ScheduledTask): string {
  if (task.schedule.kind === 'one_shot') {
    return `Once, ${new Date(task.next_fire).toLocaleString()}`;
  }
  const { amount, unit } = secondsToAmountUnit(task.schedule.interval_secs);
  return `Every ${amount} ${unit} · next ${new Date(task.next_fire).toLocaleString()}`;
}

interface FormState {
  name: string;
  prompt: string;
  cwd: string;
  /** Empty = no override, run on whatever provider is active at fire time. */
  modelId: string;
  kind: 'one_shot' | 'recurring';
  oneShotAt: string; // datetime-local value
  intervalAmount: number;
  intervalUnit: IntervalUnit;
  enabled: boolean;
}

function blankForm(): FormState {
  const in5min = new Date(Date.now() + 5 * 60_000);
  return {
    name: '',
    prompt: '',
    cwd: '',
    modelId: '',
    kind: 'one_shot',
    oneShotAt: toDatetimeLocalValue(in5min.toISOString()),
    intervalAmount: 1,
    intervalUnit: 'hours',
    enabled: true,
  };
}

function formFromTask(task: ScheduledTask): FormState {
  const { amount, unit } =
    task.schedule.kind === 'recurring'
      ? secondsToAmountUnit(task.schedule.interval_secs)
      : { amount: 1, unit: 'hours' as IntervalUnit };
  return {
    name: task.name,
    prompt: task.prompt,
    cwd: task.cwd ?? '',
    modelId: task.model_id ?? '',
    kind: task.schedule.kind,
    oneShotAt: toDatetimeLocalValue(task.next_fire),
    intervalAmount: amount,
    intervalUnit: unit,
    enabled: task.enabled,
  };
}

/** Settings panel for scheduled tasks — an instruction the agent runs later,
    one-shot or recurring, with or without the app open (fired by
    `lifecycle::spawn_scheduler_loop`). Always starts a brand-new session in
    `cwd` (or the app default) — never a persistent, context-accumulating one,
    by design (simpler and predictable; avoids an unbounded context window for
    a background task nobody's actively pruning). */
export function ScheduledTasks() {
  const [tasks, setTasks] = useState<ScheduledTask[]>([]);
  const [error, setError] = useState('');
  const [editing, setEditing] = useState<ScheduledTask | 'new' | null>(null);
  const [form, setForm] = useState<FormState>(blankForm());
  const [saving, setSaving] = useState(false);
  const [models, setModels] = useState<LocalModel[]>([]);

  const load = async () => {
    try {
      setTasks(await ipc.listScheduledTasks());
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    void load();
    // Best-effort: an empty list just means the picker offers only "whatever
    // is active", which is the pre-override behaviour anyway.
    void ipc
      .listLocalModels()
      .then(setModels)
      .catch(() => {});
    const un = onScheduledTasksChanged(() => void load());
    return () => void un.then((fn) => fn());
  }, []);

  const openNew = () => {
    setForm(blankForm());
    setEditing('new');
  };

  const openEdit = (t: ScheduledTask) => {
    setForm(formFromTask(t));
    setEditing(t);
  };

  const save = async () => {
    const name = form.name.trim();
    const prompt = form.prompt.trim();
    if (!name || !prompt) {
      setError('Name and prompt are both required.');
      return;
    }
    setSaving(true);
    setError('');
    try {
      const cwd = form.cwd.trim() || null;
      const modelId = form.modelId.trim() || null;
      // The backend deserializes `interval_secs` as a u64: a fractional
      // amount (1.1 minutes → 66.000…01) is rejected with a cryptic serde
      // error, and anything under a minute is more heat than light for a
      // background agent task — round to whole seconds and clamp at 60.
      const intervalSecs = Math.max(
        60,
        Math.round(form.intervalAmount * UNIT_SECONDS[form.intervalUnit])
      );
      const schedule: Schedule =
        form.kind === 'one_shot'
          ? { kind: 'one_shot' }
          : {
              kind: 'recurring',
              interval_secs: intervalSecs,
            };
      if (form.kind === 'one_shot') {
        // The datetime-local input is user-editable and can be cleared to ''
        // — new Date('') is Invalid Date and .toISOString() throws a
        // RangeError that would surface as a terse generic save failure.
        // Validate with a precise message instead.
        const when = new Date(form.oneShotAt);
        if (Number.isNaN(when.getTime())) {
          setError('Pick a date/time for the one-shot task.');
          return;
        }
      }
      const nextFire =
        form.kind === 'one_shot'
          ? new Date(form.oneShotAt).toISOString()
          : new Date(Date.now() + intervalSecs * 1000).toISOString();

      if (editing === 'new') {
        await ipc.createScheduledTask(name, prompt, cwd, modelId, schedule, nextFire);
      } else if (editing) {
        await ipc.updateScheduledTask(
          editing.id,
          name,
          prompt,
          cwd,
          modelId,
          schedule,
          nextFire,
          form.enabled
        );
      }
      setEditing(null);
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const remove = async (t: ScheduledTask) => {
    if (!confirm(`Delete scheduled task "${t.name}"? This cannot be undone.`)) return;
    try {
      await ipc.deleteScheduledTask(t.id);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const toggleEnabled = async (t: ScheduledTask) => {
    try {
      await ipc.setScheduledTaskEnabled(t.id, !t.enabled);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-section">
      <h1>Scheduled Tasks</h1>
      <p className="muted">
        Give the agent an instruction to run later — once, or on a repeating interval. Each run
        starts a brand-new session, with or without Kitty's window open.
      </p>
      {error && <div className="chat-error">{error}</div>}
      {tasks.length === 0 && !error && <p className="muted">No scheduled tasks yet.</p>}
      <div className="ext-list">
        {tasks.map((t) => (
          <div className="row" key={t.id} style={{ alignItems: 'center' }}>
            <label className="check" style={{ marginRight: 4 }}>
              <input type="checkbox" checked={t.enabled} onChange={() => void toggleEnabled(t)} />
            </label>
            <div style={{ flex: 1 }}>
              <div>{t.name}</div>
              <div className="muted" style={{ fontSize: 13 }}>
                {scheduleSummary(t)}
              </div>
            </div>
            <button onClick={() => openEdit(t)}>Edit</button>
            <button onClick={() => void remove(t)}>Delete</button>
          </div>
        ))}
      </div>
      <button className="primary" onClick={openNew}>
        + New scheduled task
      </button>

      {editing && (
        <Modal
          title={editing === 'new' ? 'New scheduled task' : `Edit: ${editing.name}`}
          onClose={() => setEditing(null)}
        >
          <div className="field">
            <span>Name</span>
            <input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} />
          </div>
          <div className="field">
            <span>Prompt</span>
            <textarea
              rows={4}
              value={form.prompt}
              onChange={(e) => setForm({ ...form, prompt: e.target.value })}
              placeholder="What should the agent do when this fires?"
            />
          </div>
          <div className="field">
            <span>Working directory (optional)</span>
            <div className="row">
              <input
                value={form.cwd}
                placeholder="Default folder if left blank"
                onChange={(e) => setForm({ ...form, cwd: e.target.value })}
              />
              <button
                onClick={async () => {
                  const dir = await pickFolder();
                  if (dir) setForm({ ...form, cwd: dir });
                }}
              >
                Browse…
              </button>
            </div>
          </div>
          <div className="field">
            <span>Model (optional)</span>
            <select
              value={form.modelId}
              onChange={(e) => setForm({ ...form, modelId: e.target.value })}
            >
              <option value="">Whatever is active when it runs</option>
              {models.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.id}
                </option>
              ))}
            </select>
            <small className="muted">
              Pin a downloaded model so this task always runs on it, even if you switch providers
              later.
            </small>
          </div>
          <div className="field">
            <span>Schedule</span>
            <div className="row">
              <label className="check">
                <input
                  type="radio"
                  name="schedule-kind"
                  checked={form.kind === 'one_shot'}
                  onChange={() => setForm({ ...form, kind: 'one_shot' })}
                />
                <span>Once</span>
              </label>
              <label className="check">
                <input
                  type="radio"
                  name="schedule-kind"
                  checked={form.kind === 'recurring'}
                  onChange={() => setForm({ ...form, kind: 'recurring' })}
                />
                <span>Recurring</span>
              </label>
            </div>
          </div>
          {form.kind === 'one_shot' ? (
            <div className="field">
              <span>Run at</span>
              <input
                type="datetime-local"
                value={form.oneShotAt}
                onChange={(e) => setForm({ ...form, oneShotAt: e.target.value })}
              />
            </div>
          ) : (
            <div className="field">
              <span>Repeat every</span>
              <div className="row">
                <input
                  type="number"
                  min={1}
                  step={1}
                  value={form.intervalAmount}
                  onChange={(e) =>
                    setForm({ ...form, intervalAmount: Math.max(1, Number(e.target.value)) })
                  }
                  style={{ width: 70 }}
                />
                <select
                  value={form.intervalUnit}
                  onChange={(e) =>
                    setForm({ ...form, intervalUnit: e.target.value as IntervalUnit })
                  }
                >
                  <option value="minutes">Minutes</option>
                  <option value="hours">Hours</option>
                  <option value="days">Days</option>
                </select>
              </div>
              <small className="muted">
                Starts counting from when you save — the first run will be one interval from now.
              </small>
            </div>
          )}
          {editing !== 'new' && (
            <label className="check">
              <input
                type="checkbox"
                checked={form.enabled}
                onChange={(e) => setForm({ ...form, enabled: e.target.checked })}
              />
              <span>Enabled</span>
            </label>
          )}
          <div className="row">
            <button className="primary" disabled={saving} onClick={() => void save()}>
              {saving ? 'Saving…' : 'Save'}
            </button>
            <button onClick={() => setEditing(null)}>Cancel</button>
          </div>
        </Modal>
      )}
    </section>
  );
}
