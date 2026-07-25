import { useEffect, useState } from 'react';
import { ipc, onRecipesChanged, pickRecipeSavePath, pickRecipeYaml } from '@/lib/ipc';
import { recipeNeedsAttention } from '@/lib/recipes';
import type {
  ParameterInputType,
  ParameterRequirement,
  Recipe,
  RecipeParameter,
} from '@/lib/types';
import { Modal } from '@/components/shared/Modal';
import { WarningIcon } from '@/components/icons/WarningIcon';
import {
  ActivityRow,
  blankForm,
  blankParameter,
  DEFAULT_MAX_REASONING_TOKENS,
  FormState,
  formFromRecipe,
  formToInput,
  NON_PRIMARY_INPUT_TYPES,
  slugify,
} from './recipes/recipeForm';

/** Settings panel for Goose recipes — client-side-interpreted templates
    (instructions/extensions attached to a chat turn, not the real `goose run
    --recipe` CLI runner — see `chatStore.ts`'s `sendWithRecipe`). Mirrors
    `ScheduledTasks.tsx`'s list+modal-form CRUD shape. */
export function Recipes() {
  const [recipes, setRecipes] = useState<Recipe[]>([]);
  const [error, setError] = useState('');
  const [editing, setEditing] = useState<Recipe | 'new' | null>(null);
  const [form, setForm] = useState<FormState>(blankForm());
  const [saving, setSaving] = useState(false);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [choosingTemplate, setChoosingTemplate] = useState(false);
  const [customExt, setCustomExt] = useState({ name: '', command: '', args: '', envKeys: '' });
  const [importWarnings, setImportWarnings] = useState<string[]>([]);

  const load = async () => {
    try {
      setRecipes(await ipc.listRecipes());
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    void load();
    const un = onRecipesChanged(() => void load());
    return () => void un.then((fn) => fn());
  }, []);

  const userRecipes = recipes.filter((r) => !r.is_builtin);
  const builtinRecipes = recipes.filter((r) => r.is_builtin);

  const openNew = () => {
    setForm(blankForm());
    setEditing('new');
    setAdvancedOpen(false);
    setImportWarnings([]);
  };

  const openFromTemplate = (template: Recipe) => {
    const seeded = formFromRecipe(template);
    // Pre-fill a valid slug derived from the copy title — the title is set
    // programmatically here (not via the onChange that auto-derives), so
    // without this the slug would stay empty and an immediate Save would fail
    // "Slug can't be empty". Deriving from "Copy of …" also avoids colliding
    // with the built-in's own slug. `slugTouched: false` still lets it
    // re-derive if the author then edits the title.
    const copyTitle = `Copy of ${template.title}`;
    setForm({ ...seeded, title: copyTitle, slug: slugify(copyTitle), slugTouched: false });
    setEditing('new');
    setAdvancedOpen(false);
    setImportWarnings([]);
    setChoosingTemplate(false);
  };

  const openEdit = (r: Recipe) => {
    setForm(formFromRecipe(r));
    setEditing(r);
    setAdvancedOpen(false);
    setImportWarnings([]);
  };

  const save = async () => {
    if (!form.title.trim() || !form.description.trim()) {
      setError('Title and description are both required.');
      return;
    }
    if (!form.instructions.trim() && !form.prompt.trim()) {
      setError('At least one of Instructions or Prompt is required.');
      return;
    }
    setSaving(true);
    setError('');
    try {
      const input = formToInput(form);
      if (editing === 'new') {
        await ipc.createRecipe(input);
      } else if (editing) {
        await ipc.updateRecipe(editing.id, input);
      }
      setEditing(null);
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const duplicate = async (r: Recipe) => {
    try {
      const copy = await ipc.duplicateRecipe(r.id);
      openEdit(copy);
    } catch (e) {
      setError(String(e));
    }
  };

  const remove = async (r: Recipe) => {
    if (!confirm(`Delete recipe "${r.title}"? This cannot be undone.`)) return;
    try {
      await ipc.deleteRecipe(r.id);
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  const importYaml = async () => {
    const path = await pickRecipeYaml();
    if (!path) return;
    try {
      const result = await ipc.importRecipeYaml(path);
      setImportWarnings(result.warnings);
      openEdit(result.recipe);
    } catch (e) {
      setError(String(e));
    }
  };

  const exportYaml = async (r: Recipe) => {
    const path = await pickRecipeSavePath(`${r.slug}.yaml`);
    if (!path) return;
    try {
      await ipc.exportRecipeYaml(r.id, path);
    } catch (e) {
      setError(String(e));
    }
  };

  const setSlugFromTitle = (title: string) => {
    setForm((f) => ({ ...f, title, slug: f.slugTouched ? f.slug : slugify(title) }));
  };

  const addParameter = () =>
    setForm((f) => ({ ...f, parameters: [...f.parameters, blankParameter()] }));
  const updateParameter = (i: number, patch: Partial<RecipeParameter>) =>
    setForm((f) => ({
      ...f,
      parameters: f.parameters.map((p, idx) => (idx === i ? { ...p, ...patch } : p)),
    }));
  const removeParameter = (i: number) =>
    setForm((f) => ({ ...f, parameters: f.parameters.filter((_, idx) => idx !== i) }));

  const addCustomExtension = () => {
    if (!customExt.name.trim() || !customExt.command.trim()) return;
    setForm((f) => ({
      ...f,
      extensions: [
        ...f.extensions,
        {
          type: 'stdio',
          name: customExt.name.trim(),
          cmd: customExt.command.trim(),
          args: customExt.args.split(/\s+/).filter(Boolean),
          env_keys: customExt.envKeys
            .split(',')
            .map((s) => s.trim())
            .filter(Boolean),
        },
      ],
    }));
    setCustomExt({ name: '', command: '', args: '', envKeys: '' });
  };
  const removeExtension = (i: number) =>
    setForm((f) => ({ ...f, extensions: f.extensions.filter((_, idx) => idx !== i) }));

  const addActivity = () =>
    setForm((f) => ({ ...f, activities: [...f.activities, { text: '', isMessage: false }] }));
  const updateActivity = (i: number, patch: Partial<ActivityRow>) =>
    setForm((f) => ({
      ...f,
      activities: f.activities.map((a, idx) => (idx === i ? { ...a, ...patch } : a)),
    }));
  const removeActivity = (i: number) =>
    setForm((f) => ({ ...f, activities: f.activities.filter((_, idx) => idx !== i) }));

  const editingBuiltin = editing !== 'new' && editing !== null && editing.is_builtin;

  return (
    <section className="settings-section">
      <h1>Recipes</h1>
      <p className="muted">
        A recipe attaches instructions, extensions, and a starting prompt to a chat message — invoke
        one with <code>/slug your request</code> in the composer (typing <code>/</code> shows a
        list). Built-in templates are read-only starting points; duplicate one to make your own
        editable copy.
      </p>
      {error && <div className="chat-error">{error}</div>}

      {builtinRecipes.length > 0 && (
        <>
          <h2 style={{ fontSize: 13, marginTop: 12 }}>Built-in templates</h2>
          <div className="ext-list">
            {builtinRecipes.map((r) => (
              <div className="row" key={r.id} style={{ alignItems: 'center' }}>
                <div style={{ flex: 1 }}>
                  <div>
                    /{r.slug} — {r.title}
                  </div>
                  <div className="muted" style={{ fontSize: 11 }}>
                    {r.description}
                  </div>
                </div>
                <span className="muted" style={{ fontSize: 11 }}>
                  Built-in
                </span>
                <button onClick={() => void duplicate(r)}>Duplicate as new recipe</button>
              </div>
            ))}
          </div>
        </>
      )}

      <h2 style={{ fontSize: 13, marginTop: 12 }}>Your recipes</h2>
      {userRecipes.length === 0 && !error && <p className="muted">No custom recipes yet.</p>}
      <div className="ext-list">
        {userRecipes.map((r) => {
          const missing = recipeNeedsAttention(r);
          return (
            <div className="row" key={r.id} style={{ alignItems: 'center' }}>
              <div style={{ flex: 1 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                  {missing.length > 0 && (
                    <span title={`Needs attention: ${missing.join(', ')}`}>
                      <WarningIcon />
                    </span>
                  )}
                  /{r.slug} — {r.title}
                </div>
                <div className="muted" style={{ fontSize: 11 }}>
                  {r.description}
                </div>
              </div>
              <button onClick={() => openEdit(r)}>Edit</button>
              <button onClick={() => void duplicate(r)}>Duplicate</button>
              <button onClick={() => void exportYaml(r)}>Export</button>
              <button onClick={() => void remove(r)}>Delete</button>
            </div>
          );
        })}
      </div>
      <div className="row">
        <button className="primary" onClick={() => setChoosingTemplate(true)}>
          + New recipe
        </button>
        <button onClick={() => void importYaml()}>Import YAML…</button>
      </div>

      {choosingTemplate && (
        <Modal title="New recipe">
          <p className="muted">Start from a template, or start blank.</p>
          <div className="ext-grid">
            {builtinRecipes.map((r) => (
              <button
                key={r.id}
                className="ext-card"
                style={{ textAlign: 'left', cursor: 'pointer' }}
                onClick={() => openFromTemplate(r)}
              >
                <div className="ext-card-head">
                  <span className="ext-card-name">{r.title}</span>
                </div>
                <span className="muted ext-card-desc">{r.description}</span>
              </button>
            ))}
          </div>
          <div className="row" style={{ marginTop: 12 }}>
            <button
              className="primary"
              onClick={() => {
                setChoosingTemplate(false);
                openNew();
              }}
            >
              Start blank
            </button>
            <button onClick={() => setChoosingTemplate(false)}>Cancel</button>
          </div>
        </Modal>
      )}

      {editing && (
        <Modal
          title={
            editingBuiltin
              ? `Built-in: ${(editing as Recipe).title}`
              : editing === 'new'
                ? 'New recipe'
                : `Edit: ${editing.title}`
          }
        >
          {importWarnings.length > 0 && (
            <div className="chat-error" role="alert">
              {importWarnings.map((w, i) => (
                <div key={i}>{w}</div>
              ))}
              <button className="link" onClick={() => setImportWarnings([])}>
                Dismiss
              </button>
            </div>
          )}
          <div className="field">
            <span>Title</span>
            <input
              value={form.title}
              disabled={editingBuiltin}
              onChange={(e) => setSlugFromTitle(e.target.value)}
            />
          </div>
          <div className="field">
            <span>Description</span>
            <input
              value={form.description}
              disabled={editingBuiltin}
              onChange={(e) => setForm({ ...form, description: e.target.value })}
            />
          </div>
          <div className="field">
            <span>Slash command — /{form.slug || 'slug'}</span>
            <input
              value={form.slug}
              disabled={editingBuiltin}
              onChange={(e) =>
                setForm({
                  ...form,
                  slug: e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, ''),
                  slugTouched: true,
                })
              }
            />
          </div>
          <div className="field">
            <span>Instructions (sent as a hidden preamble — the model's behavior/rules)</span>
            <textarea
              rows={5}
              value={form.instructions}
              disabled={editingBuiltin}
              onChange={(e) => setForm({ ...form, instructions: e.target.value })}
            />
          </div>
          <div className="field">
            <span>Prompt template (the message that kicks off the request — optional)</span>
            <textarea
              rows={3}
              value={form.prompt}
              disabled={editingBuiltin}
              placeholder="Leave blank to send exactly what the user types after /slug"
              onChange={(e) => setForm({ ...form, prompt: e.target.value })}
            />
          </div>
          <div className="field">
            <span>What should the user type after /{form.slug || 'slug'}?</span>
            <div className="row">
              <input
                value={form.primaryKey}
                disabled={editingBuiltin}
                placeholder="Variable name, e.g. request"
                style={{ maxWidth: 160 }}
                onChange={(e) =>
                  setForm({
                    ...form,
                    primaryKey: e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, ''),
                  })
                }
              />
            </div>
            <textarea
              rows={2}
              value={form.primaryDescription}
              disabled={editingBuiltin}
              placeholder="Guidance shown when invoking — e.g. 'The debate motion, and each debater's persona (e.g. ...).'"
              onChange={(e) => setForm({ ...form, primaryDescription: e.target.value })}
            />
            <small className="muted">
              Used as <code>{`{{${form.primaryKey || 'request'}}}`}</code> in Instructions/Prompt.
              Shown to whoever invokes <code>/{form.slug || 'slug'}</code> — write it as a worked
              example of everything the recipe can use, not just the topic.
            </small>
          </div>

          <button
            type="button"
            className="disclosure-toggle"
            onClick={() => setAdvancedOpen((o) => !o)}
          >
            {advancedOpen ? '▾' : '▸'} Advanced
          </button>
          {advancedOpen && (
            <div className="provider-advanced-body">
              <div className="field">
                <span>Other parameters (fixed values — not collected at invocation time)</span>
                {form.parameters.map((p, i) => (
                  <div className="row" key={i} style={{ alignItems: 'center', flexWrap: 'wrap' }}>
                    <input
                      placeholder="key"
                      value={p.key}
                      disabled={editingBuiltin}
                      style={{ width: 100 }}
                      onChange={(e) => updateParameter(i, { key: e.target.value })}
                    />
                    <select
                      value={p.input_type}
                      disabled={editingBuiltin}
                      onChange={(e) =>
                        updateParameter(i, {
                          input_type: e.target.value as ParameterInputType,
                        })
                      }
                    >
                      {NON_PRIMARY_INPUT_TYPES.map((t) => (
                        <option key={t} value={t}>
                          {t}
                        </option>
                      ))}
                    </select>
                    <select
                      value={p.requirement}
                      disabled={editingBuiltin}
                      onChange={(e) =>
                        updateParameter(i, {
                          requirement: e.target.value as ParameterRequirement,
                        })
                      }
                    >
                      <option value="optional">optional</option>
                      <option value="required">required</option>
                    </select>
                    <input
                      placeholder="description"
                      value={p.description}
                      disabled={editingBuiltin}
                      style={{ flex: 1, minWidth: 100 }}
                      onChange={(e) => updateParameter(i, { description: e.target.value })}
                    />
                    {p.input_type === 'select' ? (
                      <input
                        placeholder="options (comma-separated)"
                        value={p.options.join(',')}
                        disabled={editingBuiltin}
                        onChange={(e) =>
                          updateParameter(i, {
                            options: e.target.value
                              .split(',')
                              .map((s) => s.trim())
                              .filter(Boolean),
                          })
                        }
                      />
                    ) : (
                      <input
                        placeholder="default"
                        value={p.default ?? ''}
                        disabled={editingBuiltin}
                        onChange={(e) => updateParameter(i, { default: e.target.value })}
                      />
                    )}
                    {!editingBuiltin && <button onClick={() => removeParameter(i)}>Remove</button>}
                  </div>
                ))}
                {!editingBuiltin && <button onClick={addParameter}>+ Parameter</button>}
              </div>

              <div className="field">
                <span>Extensions</span>
                {form.extensions.filter((e) => e.type === 'stdio').length > 0 && (
                  <div className="ext-list">
                    {form.extensions.map((e, i) =>
                      e.type === 'stdio' ? (
                        <div className="row" key={i} style={{ alignItems: 'center' }}>
                          <span style={{ flex: 1 }}>
                            {e.name} — {e.cmd}
                          </span>
                          {!editingBuiltin && (
                            <button onClick={() => removeExtension(i)}>Remove</button>
                          )}
                        </div>
                      ) : null
                    )}
                  </div>
                )}
                {!editingBuiltin && (
                  <>
                    <div className="row">
                      <input
                        placeholder="Name"
                        value={customExt.name}
                        onChange={(e) => setCustomExt({ ...customExt, name: e.target.value })}
                      />
                      <input
                        placeholder="Command"
                        value={customExt.command}
                        onChange={(e) => setCustomExt({ ...customExt, command: e.target.value })}
                      />
                    </div>
                    <input
                      placeholder="Args (space-separated)"
                      value={customExt.args}
                      onChange={(e) => setCustomExt({ ...customExt, args: e.target.value })}
                    />
                    <input
                      placeholder="Env var names needed (comma-separated)"
                      value={customExt.envKeys}
                      onChange={(e) => setCustomExt({ ...customExt, envKeys: e.target.value })}
                    />
                    <button onClick={addCustomExtension}>+ Custom extension</button>
                  </>
                )}
              </div>

              <div className="field">
                <span>Activities (suggested prompts, shown in the recipe list)</span>
                {form.activities.map((a, i) => (
                  <div className="row" key={i} style={{ alignItems: 'center' }}>
                    <input
                      value={a.text}
                      disabled={editingBuiltin}
                      style={{ flex: 1 }}
                      onChange={(e) => updateActivity(i, { text: e.target.value })}
                    />
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={a.isMessage}
                        disabled={editingBuiltin}
                        onChange={(e) => updateActivity(i, { isMessage: e.target.checked })}
                      />
                      <span>Info message, not a button</span>
                    </label>
                    {!editingBuiltin && <button onClick={() => removeActivity(i)}>Remove</button>}
                  </div>
                ))}
                {!editingBuiltin && <button onClick={addActivity}>+ Activity</button>}
              </div>

              <div className="field">
                <span>Reasoning cap (tokens)</span>
                <input
                  type="number"
                  min={1}
                  value={form.maxReasoningTokens}
                  disabled={editingBuiltin}
                  style={{ maxWidth: 120 }}
                  onChange={(e) =>
                    setForm({ ...form, maxReasoningTokens: Number(e.target.value) })
                  }
                />
                <small className="muted">
                  If this recipe's response reasons past this many tokens (estimated), Kitty
                  stops it automatically — a hard limit, not just the usual loop-detection
                  suggestion (which is skipped for recipe turns, since some recipes — like the
                  debate moderator — legitimately produce long, structurally-repetitive output).
                  There's no per-model maximum to read here, so this defaults to a conservative{' '}
                  {DEFAULT_MAX_REASONING_TOKENS}.
                </small>
              </div>
            </div>
          )}

          <div className="row" style={{ marginTop: 12 }}>
            {editingBuiltin ? (
              // `duplicate()` already transitions the modal to the new,
              // fully-editable copy (via `openEdit`) — don't chain a
              // `setEditing(null)` that would immediately close it.
              <button className="primary" onClick={() => void duplicate(editing as Recipe)}>
                Duplicate as new recipe
              </button>
            ) : (
              <button className="primary" disabled={saving} onClick={() => void save()}>
                {saving ? 'Saving…' : 'Save'}
              </button>
            )}
            <button onClick={() => setEditing(null)}>Cancel</button>
          </div>
        </Modal>
      )}
    </section>
  );
}
