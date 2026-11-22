import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { ProviderView, Specialist, SpecialistRun } from '@/lib/types';
import { setupTitle } from '@/components/settings/setups/setupUtils';
import { DelegateRunRow } from './DelegateRunRow';
import { CritiquePanel } from './CritiquePanel';
import { DebatePanel } from './DebatePanel';
import { Modal } from '@/components/shared/Modal';
import { confirmDialog } from '@/components/shared/ConfirmDialog';

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
  /** Runs of this specialist at once; '' = only the global limit. */
  maxConcurrent: string;
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
    maxConcurrent: '',
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
    maxConcurrent: s.max_concurrent != null ? String(s.max_concurrent) : '',
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
  // Overall limits on delegate runs; start-up settings of the engine.
  const [limits, setLimits] = useState<{ timeout: string; concurrent: string } | null>(null);
  const [limitsSaved, setLimitsSaved] = useState(false);
  const saveLimits = async () => {
    if (!limits) return;
    const timeout = Math.round(Number(limits.timeout));
    const concurrent = Math.round(Number(limits.concurrent));
    if (!(timeout >= 30) || !(concurrent >= 1)) {
      setError('Give a time limit of at least 30 seconds and at least one run at a time.');
      return;
    }
    setError('');
    try {
      await ipc.patchConfig({ specialists: { timeout_secs: timeout, max_concurrent: concurrent } });
      setLimitsSaved(true);
    } catch (e) {
      setError(String(e));
    }
  };
  const [denyDraft, setDenyDraft] = useState('');
  // Global master switch (distinct from each specialist's own `enabled`): when
  // off, the whole delegation server is unregistered and the model can't
  // delegate at all.
  const [masterEnabled, setMasterEnabled] = useState(true);

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
      .then((c) => {
        setDeny(c.specialists?.model_deny ?? []);
        setLimits({
          timeout: String(c.specialists.timeout_secs),
          concurrent: String(c.specialists.max_concurrent),
        });
      })
      .catch(() => {});
    void ipc
      .getSpecialistsEnabled()
      .then(setMasterEnabled)
      .catch(() => {});
  }, []);

  const toggleMaster = async (next: boolean) => {
    setMasterEnabled(next); // optimistic
    try {
      await ipc.setSpecialistsEnabled(next);
    } catch (e) {
      setMasterEnabled(!next); // revert on failure
      setError(String(e));
    }
  };

  const saveDeny = async (next: string[]) => {
    try {
      // `seeded` stays true so clearing the list is not undone on the next
      // launch — an empty list the user chose is a decision, not an absence.
      await ipc.patchConfig({ specialists: { model_deny: next, seeded: true } });
      setDeny(next);
    } catch (e) {
      setError(String(e));
    }
  };

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
        max_concurrent: form.maxConcurrent.trim() ? Number(form.maxConcurrent) : null,
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
    const ok = await confirmDialog({
      title: `Delete "${s.name}"?`,
      message: 'The model can no longer delegate to this specialist. This cannot be undone.',
      confirmLabel: 'Delete',
      danger: true,
    });
    if (!ok) return;
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
        Helpers your chat&apos;s model can hand a piece of work to, such as researching a question.
        Their searching and reading happens off to the side, and only the answer comes back into
        your chat. The model decides when to use one.
      </p>
      <label className="check">
        <input
          type="checkbox"
          checked={masterEnabled}
          onChange={(e) => void toggleMaster(e.target.checked)}
        />
        Enable specialists
      </label>
      <p className="muted" style={{ margin: 0 }}>
        Off: the model does everything itself. Your specialists are kept.
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

      <div className="settings-subhead">
        <h2>Models specialists never use</h2>
        <p className="muted">
          Keeps expensive models off background work. Exact model ids, or a prefix such as{' '}
          <code>claude-opus-*</code>. Applies once Kitty restarts its engine, which it does as soon
          as nothing is running.
        </p>
      </div>
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

      <div className="settings-subhead">
        <h2>Limits</h2>
      </div>
      {limits && (
        <>
          <div className="row">
            <label className="field">
              <span>Time limit per run (seconds)</span>
              <input
                type="number"
                min={30}
                value={limits.timeout}
                onChange={(e) => {
                  setLimitsSaved(false);
                  setLimits({ ...limits, timeout: e.target.value });
                }}
              />
            </label>
            <label className="field">
              <span>Runs at once, all specialists</span>
              <input
                type="number"
                min={1}
                max={16}
                value={limits.concurrent}
                onChange={(e) => {
                  setLimitsSaved(false);
                  setLimits({ ...limits, concurrent: e.target.value });
                }}
              />
            </label>
          </div>
          <div className="row">
            <button onClick={() => void saveLimits()}>Save limits</button>
            {limitsSaved && (
              <span className="muted">Saved — applies when the engine next restarts.</span>
            )}
          </div>
        </>
      )}

      <div className="settings-subhead">
        <h2>Recent runs</h2>
        <p className="muted">
          The last 10 jobs: which specialist did what, on which model. Open one to read its work. A
          specialist that keeps getting the wrong jobs needs a sharper description.
        </p>
      </div>
      {runs.length === 0 ? (
        <p className="muted">Nothing delegated yet.</p>
      ) : (
        <div className="ext-list">
          {runs.slice(0, 10).map((r) => (
            <DelegateRunRow key={r.id} run={r} />
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
              The model reads only this to decide when to use the specialist. Say what it is for,
              and what it is not for.
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
              The only tools it may use. None checked: it only thinks.
            </small>
          </div>
          <div className="field">
            <span>Model (optional)</span>
            {/* Each entry is a setup (its card: the engine row the specialist
                is pinned to), so the model comes with it. The specialist's own
                prompt replaces the setup's. */}
            <select
              value={form.provider}
              onChange={(e) => {
                const card = providers.find((p) => p.id === e.target.value);
                setForm({ ...form, provider: e.target.value, model: card?.models[0] ?? '' });
              }}
            >
              <option value="">Whatever this chat is using</option>
              {providers.map((p) => (
                <option key={p.id} value={p.id} title={setupTitle(p)}>
                  {p.name}
                </option>
              ))}
            </select>
            <small className="muted">
              A cheaper or faster model for work that doesn&apos;t need your main one. The
              specialist&apos;s instructions replace the model&apos;s system prompt.
            </small>
          </div>
          <div className="field">
            <span>Answer schema (optional)</span>
            <textarea
              rows={6}
              value={form.responseSchema}
              onChange={(e) => setForm({ ...form, responseSchema: e.target.value })}
              placeholder='{"type": "object", "properties": {…}, "required": […], "additionalProperties": false}'
            />
            <small className="muted">
              A JSON Schema for a short, exact answer; without one it answers in prose. (OpenAI
              models need every property in <code>required</code> and{' '}
              <code>additionalProperties: false</code>.)
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
              How much it may think before answering: tokens, or a share of its context such as{' '}
              <code>25%</code>. Blank uses the default.
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
            Several documents are handled one per copy, in parallel. Right for per-document work;
            wrong for anything that compares across them.
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
            <small className="muted">How many rounds of tool use before it must answer.</small>
          </div>
          <div className="field">
            <span>At most this many at once (optional)</span>
            <input
              type="number"
              min={1}
              max={16}
              placeholder="No limit of its own"
              value={form.maxConcurrent}
              onChange={(e) => setForm({ ...form, maxConcurrent: e.target.value })}
            />
            <small className="muted">
              For a provider that limits parallel requests. The overall limit still applies.
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
          {/* Kitty's own two specialists carry settings of their own, kept
              with them rather than as panels of the Specialists page. They
              save on their own; Save below is for the specialist. */}
          {editing !== 'new' && editing.name === 'debater' && (
            <div className="specialist-extra">
              <DebatePanel />
            </div>
          )}
          {editing !== 'new' && editing.name === 'critic' && (
            <div className="specialist-extra">
              <CritiquePanel />
            </div>
          )}
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
