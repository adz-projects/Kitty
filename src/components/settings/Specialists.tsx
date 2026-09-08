import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { ProviderView, Specialist, SpecialistRun } from '@/lib/types';
import { Modal } from '@/components/shared/Modal';

interface FormState {
  name: string;
  description: string;
  systemPrompt: string;
  provider: string;
  model: string;
  toolAllow: string[];
  /** Raw JSON text, validated on save. Empty = prose answer, no schema. */
  responseSchema: string;
  maxSteps: number;
  /** Empty inherits the daemon default. A number is absolute tokens; a value
      ending in `%` is a share of the delegate's remaining context. */
  reasoningCap: string;
  fanOut: boolean;
  enabled: boolean;
}

function blankForm(): FormState {
  return {
    name: '',
    description: '',
    systemPrompt: '',
    provider: '',
    model: '',
    toolAllow: [],
    responseSchema: '',
    maxSteps: 20,
    reasoningCap: '',
    fanOut: false,
    enabled: true,
  };
}

function formFromSpecialist(s: Specialist): FormState {
  return {
    name: s.name,
    description: s.description,
    systemPrompt: s.system_prompt,
    provider: s.provider ?? '',
    model: s.model ?? '',
    toolAllow: [...s.tool_allow],
    responseSchema: s.response_schema ? JSON.stringify(s.response_schema, null, 2) : '',
    maxSteps: s.max_steps || 20,
    reasoningCap:
      s.reasoning_cap_tokens != null
        ? String(s.reasoning_cap_tokens)
        : s.reasoning_cap_fraction != null
          ? `${Math.round(s.reasoning_cap_fraction * 100)}%`
          : '',
    fanOut: s.fan_out === 'per_ref',
    enabled: s.enabled,
  };
}

/** Settings panel for specialists — the delegate agents the model can call
    mid-turn to do bounded work in their own context.

    Replaces the Recipes panel. A recipe was a prompt template the user invoked
    by a `/slug`; a specialist is chosen by the model from an ordinary request,
    so there is no invocation syntax here — only a definition. That makes
    `description` the field that decides whether a specialist is ever used at
    all, which is why the form leads with it and says so.

    Built-ins are seeded by the daemon and shared by every app: they can be
    overridden (saving one of the same name creates this app's private copy)
    but never deleted. */
export function Specialists() {
  const [specialists, setSpecialists] = useState<Specialist[]>([]);
  const [tools, setTools] = useState<string[]>([]);
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [error, setError] = useState('');
  const [editing, setEditing] = useState<Specialist | 'new' | null>(null);
  const [form, setForm] = useState<FormState>(blankForm());
  const [saving, setSaving] = useState(false);
  const [runs, setRuns] = useState<SpecialistRun[]>([]);
  const [deny, setDeny] = useState<string[]>([]);
  const [denyDraft, setDenyDraft] = useState('');

  const load = async () => {
    try {
      setSpecialists(await ipc.listSpecialists());
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    void load();
    // Both best-effort: without them the form degrades to "whatever provider is
    // active" and an empty tool checklist, which is still a usable definition.
    void ipc
      .listAvailableTools()
      .then(setTools)
      .catch(() => {});
    void ipc
      .listProviders()
      .then(setProviders)
      .catch(() => {});
    void ipc
      .listSpecialistRuns()
      .then(setRuns)
      .catch(() => {});
    // The denylist lives in Kitty's own config, not the daemon's: it is relayed
    // as an env var at spawn, because a spend guard a running daemon could be
    // talked out of over HTTP would be a weaker one.
    void ipc
      .getConfig()
      .then((c) => setDeny(c.specialists?.model_deny ?? []))
      .catch(() => {});
  }, []);

  const saveDeny = async (next: string[]) => {
    try {
      const cfg = await ipc.getConfig();
      await ipc.setConfig({
        ...cfg,
        // `seeded` stays true so clearing the list is not undone on the next
        // launch — an empty list the user chose is a decision, not an absence.
        specialists: { ...cfg.specialists, model_deny: next, seeded: true },
      });
      setDeny(next);
    } catch (e) {
      setError(String(e));
    }
  };

  // Models come from the provider profile itself rather than a second lookup:
  // the daemon's provider row is keyed by the Kitty profile id (see
  // `bigtiny::providers`), and the profile already carries its model list.
  const models = providers.find((p) => p.id === form.provider)?.models ?? [];

  const openNew = () => {
    setForm(blankForm());
    setEditing('new');
  };

  const openEdit = (s: Specialist) => {
    setForm(formFromSpecialist(s));
    setEditing(s);
  };

  const toggleTool = (tool: string) => {
    setForm((f) => ({
      ...f,
      toolAllow: f.toolAllow.includes(tool)
        ? f.toolAllow.filter((t) => t !== tool)
        : [...f.toolAllow, tool],
    }));
  };

  const save = async () => {
    const name = form.name.trim();
    const description = form.description.trim();
    if (!name || !description) {
      setError('Name and description are both required.');
      return;
    }
    // Parsed here rather than sent as text: a malformed schema would otherwise
    // reach the daemon as a string and be stored as one, and the first sign of
    // trouble would be a delegate whose answers stopped validating.
    let responseSchema: unknown | null = null;
    if (form.responseSchema.trim()) {
      try {
        responseSchema = JSON.parse(form.responseSchema);
      } catch (e) {
        setError(`Response schema is not valid JSON: ${String(e)}`);
        return;
      }
    }
    // One field, two meanings: "8000" is a token count and "25%" is a share of
    // whatever window the delegate lands on. A definition that has to work
    // across an 8k local model and a 200k hosted one wants the second; a
    // pipeline that has measured its own workload wants the first.
    const raw = form.reasoningCap.trim();
    let cap: { tokens: number | null; fraction: number | null } = {
      tokens: null,
      fraction: null,
    };
    if (raw) {
      if (raw.endsWith('%')) {
        const pct = Number(raw.slice(0, -1));
        if (!Number.isFinite(pct) || pct <= 0 || pct > 100) {
          setError('Reasoning limit: use a percentage between 1 and 100, or a token count.');
          return;
        }
        cap = { tokens: null, fraction: pct / 100 };
      } else {
        const n = Number(raw);
        if (!Number.isFinite(n) || n < 0) {
          setError('Reasoning limit: use a token count, or a percentage like 25%.');
          return;
        }
        cap = { tokens: Math.round(n), fraction: null };
      }
    }

    setSaving(true);
    setError('');
    try {
      await ipc.saveSpecialist({
        name,
        description,
        system_prompt: form.systemPrompt,
        provider: form.provider.trim() || null,
        model: form.model.trim() || null,
        tool_allow: form.toolAllow,
        response_schema: responseSchema,
        max_steps: form.maxSteps,
        reasoning_cap_tokens: cap.tokens,
        reasoning_cap_fraction: cap.fraction,
        fan_out: form.fanOut ? 'per_ref' : null,
        enabled: form.enabled,
      });
      setEditing(null);
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const remove = async (s: Specialist) => {
    if (!confirm(`Delete specialist "${s.name}"? This cannot be undone.`)) return;
    try {
      await ipc.deleteSpecialist(s.id);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-section">
      <h1>Specialists</h1>
      <p className="muted">
        Delegate agents the model can call mid-turn. A specialist runs in its own context with its
        own tools, so its searching and reading never fills up your conversation — only its result
        comes back. You never invoke one directly; the model picks one when a request calls for it.
      </p>
      {error && <div className="chat-error">{error}</div>}
      <div className="ext-list">
        {specialists.map((s) => (
          <div className="row" key={s.id} style={{ alignItems: 'center' }}>
            <div style={{ flex: 1 }}>
              <div>
                {s.name}
                {s.builtin && <span className="muted"> · built-in</span>}
                {!s.enabled && <span className="muted"> · off</span>}
              </div>
              <div className="muted" style={{ fontSize: 13 }}>
                {s.description}
              </div>
            </div>
            <button onClick={() => openEdit(s)}>{s.builtin ? 'Override' : 'Edit'}</button>
            {/* Built-ins are shared by every app, so there is nothing here for
                one app to delete. Overriding is the way to change one. */}
            {!s.builtin && <button onClick={() => void remove(s)}>Delete</button>}
          </div>
        ))}
      </div>
      <button className="primary" onClick={openNew}>
        + New specialist
      </button>

      <h2>Never use as a subagent</h2>
      <p className="muted">
        Models that may never host a specialist, however the daemon would otherwise pick. Exact ids,
        or a prefix like <code>claude-fable-*</code>. This is checked last as well as first, so a
        denied model is refused even when it is the only one left — you will be told rather than
        quietly billed. Takes effect next time the backend restarts.
      </p>
      <div className="ext-list">
        {deny.map((m) => (
          <div className="row" key={m} style={{ alignItems: 'center' }}>
            <span style={{ flex: 1, fontFamily: 'var(--mono, ui-monospace, monospace)' }}>{m}</span>
            <button onClick={() => void saveDeny(deny.filter((d) => d !== m))}>Remove</button>
          </div>
        ))}
        {deny.length === 0 && <p className="muted">Nothing denied.</p>}
      </div>
      <div className="row">
        <input
          value={denyDraft}
          placeholder="claude-fable-*"
          onChange={(e) => setDenyDraft(e.target.value)}
        />
        <button
          onClick={() => {
            const v = denyDraft.trim();
            if (!v || deny.includes(v)) return;
            setDenyDraft('');
            void saveDeny([...deny, v]);
          }}
        >
          Add
        </button>
      </div>

      <h2>Recent delegate runs</h2>
      <p className="muted">
        Which specialist answered which request, and on what. The model chooses a specialist by
        reading its description, so this is where a description that is drawing the wrong work
        becomes visible.
      </p>
      {runs.length === 0 ? (
        <p className="muted">Nothing delegated yet.</p>
      ) : (
        <div className="ext-list">
          {runs.slice(0, 20).map((r) => (
            <div className="row" key={r.id} style={{ alignItems: 'center' }}>
              <span
                className={
                  r.status === 'failed'
                    ? 'status-dot bad'
                    : r.status === 'completed'
                      ? 'status-dot ok'
                      : 'status-dot warn'
                }
              />
              <div style={{ flex: 1 }}>
                <div>{r.specialist ?? 'specialist'}</div>
                <div className="muted" style={{ fontSize: 13 }}>
                  {r.summary ?? r.status}
                </div>
              </div>
              <span className="muted" style={{ fontSize: 12 }}>
                {r.started_at ? new Date(r.started_at).toLocaleString() : ''}
              </span>
            </div>
          ))}
        </div>
      )}

      {editing && (
        <Modal
          title={
            editing === 'new'
              ? 'New specialist'
              : editing.builtin
                ? `Override built-in: ${editing.name}`
                : `Edit: ${editing.name}`
          }
          onClose={() => setEditing(null)}
        >
          <div className="field">
            <span>Name</span>
            <input
              value={form.name}
              disabled={editing !== 'new'}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
            />
            {editing !== 'new' && editing.builtin && (
              <small className="muted">
                Saving keeps the built-in&apos;s name and creates your own version of it. The
                original stays available to other apps.
              </small>
            )}
          </div>
          <div className="field">
            <span>Description</span>
            <textarea
              rows={3}
              value={form.description}
              onChange={(e) => setForm({ ...form, description: e.target.value })}
              placeholder="Answers a factual question that needs sources from the web…"
            />
            <small className="muted">
              This is the only thing the model reads when deciding whether to delegate. Say what
              this specialist is for and, where it matters, what it is not for — a vague description
              does not fail loudly, it just gets used for the wrong things.
            </small>
          </div>
          <div className="field">
            <span>Instructions</span>
            <textarea
              rows={6}
              value={form.systemPrompt}
              onChange={(e) => setForm({ ...form, systemPrompt: e.target.value })}
              placeholder="How this specialist should work."
            />
          </div>
          <div className="field">
            <span>Tools</span>
            {tools.length === 0 ? (
              <small className="muted">
                No tools available — check that your MCP servers are connected.
              </small>
            ) : (
              <div className="ext-grid">
                {tools.map((t) => (
                  <label className="check" key={t}>
                    <input
                      type="checkbox"
                      checked={form.toolAllow.includes(t)}
                      onChange={() => toggleTool(t)}
                    />
                    {t}
                  </label>
                ))}
              </div>
            )}
            <small className="muted">
              Exactly what this specialist may call — anything else is refused, even if it asks for
              it by name. Leave everything unchecked for a specialist that only reasons.
            </small>
          </div>
          <div className="field">
            <span>Provider (optional)</span>
            <select
              value={form.provider}
              onChange={(e) => setForm({ ...form, provider: e.target.value, model: '' })}
            >
              <option value="">Whatever this chat is using</option>
              {providers.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
            <small className="muted">
              Pin a cheaper or faster model for work that does not need your main one.
            </small>
          </div>
          {form.provider && (
            <div className="field">
              <span>Model (optional)</span>
              <select
                value={form.model}
                onChange={(e) => setForm({ ...form, model: e.target.value })}
              >
                <option value="">That provider&apos;s default</option>
                {models.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
              </select>
            </div>
          )}
          <div className="field">
            <span>Answer schema (optional)</span>
            <textarea
              rows={6}
              value={form.responseSchema}
              onChange={(e) => setForm({ ...form, responseSchema: e.target.value })}
              placeholder='{"type": "object", "properties": {…}, "required": […], "additionalProperties": false}'
            />
            <small className="muted">
              JSON Schema the answer must match. Without one the specialist replies in prose, which
              the calling model has to interpret — the point of delegating is usually to get back
              something small and exact. Hosted OpenAI models require every property listed in{' '}
              <code>required</code> and <code>additionalProperties: false</code>.
            </small>
          </div>
          <div className="field">
            <span>Reasoning limit</span>
            <input
              value={form.reasoningCap}
              placeholder="Default"
              onChange={(e) => setForm({ ...form, reasoningCap: e.target.value })}
            />
            <small className="muted">
              How much of the run may go on thinking — a token count, or a percentage of the
              model&apos;s remaining context like <code>25%</code>. Leave blank for the default.
              When it runs out the specialist stops thinking and writes its answer with what it has,
              rather than being cut off.
            </small>
          </div>
          <label className="check">
            <input
              type="checkbox"
              checked={form.fanOut}
              onChange={(e) => setForm({ ...form, fanOut: e.target.checked })}
            />
            Run one instance per source
          </label>
          <small className="muted">
            Splits a call carrying several documents into one delegate each, processed in parallel.
            Right for per-document work like extraction; wrong for anything that has to look{' '}
            <em>across</em> the whole set, which needs to see all of it at once.
          </small>
          <div className="field">
            <span>Step limit</span>
            <input
              type="number"
              min={1}
              max={200}
              value={form.maxSteps}
              onChange={(e) => setForm({ ...form, maxSteps: Number(e.target.value) })}
            />
            <small className="muted">
              How many tool-calling rounds it may take before it has to answer with what it has.
            </small>
          </div>
          <label className="check">
            <input
              type="checkbox"
              checked={form.enabled}
              onChange={(e) => setForm({ ...form, enabled: e.target.checked })}
            />
            Offer this specialist to the model
          </label>
          <div className="row" style={{ justifyContent: 'flex-end', marginTop: 12 }}>
            <button onClick={() => setEditing(null)}>Cancel</button>
            <button className="primary" disabled={saving} onClick={() => void save()}>
              {saving ? 'Saving…' : 'Save'}
            </button>
          </div>
        </Modal>
      )}
    </section>
  );
}
