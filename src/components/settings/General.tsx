import { useConfigDraft } from './useConfigDraft';
import { pickFolder } from '@/lib/ipc';

/** General settings backed by app config. Goose-only settings (approval mode is
    per-session; see the chat mode badge) are noted where they live elsewhere. */
export function General() {
  const { draft, update, save, saved } = useConfigDraft();
  if (!draft) return <p className="muted">Loading…</p>;

  return (
    <section className="settings-section">
      <h1>General</h1>

      <label className="field">
        <span>Default context folder</span>
        <div className="row">
          <input
            value={draft.default_context_folder ?? ''}
            placeholder="%USERPROFILE%\\Documents\\Goose"
            onChange={(e) => update({ default_context_folder: e.target.value || null })}
          />
          <button
            onClick={async () => {
              const dir = await pickFolder();
              if (dir) update({ default_context_folder: dir });
            }}
          >
            Browse…
          </button>
        </div>
      </label>

      <label className="field">
        <span>Ollama endpoint</span>
        <input
          value={draft.ollama_base_url}
          onChange={(e) => update({ ollama_base_url: e.target.value })}
        />
      </label>

      <label className="field">
        <span>Toggle hotkey</span>
        <input value={draft.hotkey} onChange={(e) => update({ hotkey: e.target.value })} />
        <small className="muted">Recording UI + Copilot key arrive in Phase 6.</small>
      </label>

      <label className="field">
        <span>Auto-summarize threshold (messages)</span>
        <input
          type="number"
          min={0}
          value={draft.auto_summarize_threshold ?? ''}
          onChange={(e) =>
            update({
              auto_summarize_threshold: e.target.value ? Number(e.target.value) : null,
            })
          }
        />
      </label>

      <label className="check">
        <input
          type="checkbox"
          checked={draft.strict_remote_mode}
          onChange={(e) => update({ strict_remote_mode: e.target.checked })}
        />
        <span>Strict mode: disable file/folder drop while a remote provider is active</span>
      </label>

      <p className="muted">
        Approval mode is per session — change it from the shield badge next to the composer.
      </p>

      <div className="row">
        <button className="primary" onClick={() => void save()}>
          Save
        </button>
        {saved && <span className="muted">Saved.</span>}
      </div>
    </section>
  );
}
