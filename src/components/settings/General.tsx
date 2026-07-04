import { useEffect, useState } from 'react';
import { useConfigDraft } from './useConfigDraft';
import { ipc, pickFolder } from '@/lib/ipc';

/** Build a tauri-global-shortcut accelerator from a keydown event. */
function accelerator(e: React.KeyboardEvent): string | null {
  const mods: string[] = [];
  if (e.ctrlKey) mods.push('Control');
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  if (e.metaKey) mods.push('Super');
  const code = e.code;
  let key: string | null = null;
  if (/^Key[A-Z]$/.test(code)) key = code.slice(3);
  else if (/^Digit[0-9]$/.test(code)) key = code.slice(5);
  else if (/^F[0-9]{1,2}$/.test(code)) key = code;
  else if (code === 'Space') key = 'Space';
  else if (['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'].includes(code))
    key = code.replace('Arrow', '');
  if (!key || mods.length === 0) return null; // require at least one modifier
  return [...mods, key].join('+');
}

/** General settings backed by app config. Goose-only settings (approval mode is
    per-session; see the chat mode badge) are noted where they live elsewhere. */
export function General() {
  const { draft, update, save, saved } = useConfigDraft();
  const [recording, setRecording] = useState(false);
  const [autostart, setAutostart] = useState(false);

  useEffect(() => {
    void ipc.getAutostart().then(setAutostart);
  }, []);

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
        <div className="row">
          <input
            value={recording ? 'Press a shortcut…' : draft.hotkey}
            readOnly={recording}
            onChange={(e) => update({ hotkey: e.target.value })}
            onKeyDown={(e) => {
              if (!recording) return;
              e.preventDefault();
              const acc = accelerator(e);
              if (acc) {
                update({ hotkey: acc });
                setRecording(false);
              }
            }}
          />
          <button onClick={() => setRecording((r) => !r)}>{recording ? 'Cancel' : 'Record'}</button>
        </div>
        <small className="muted">Save to apply. Takes effect immediately after saving.</small>
      </label>

      <label className="check">
        <input
          type="checkbox"
          checked={draft.use_copilot_key}
          onChange={(e) => update({ use_copilot_key: e.target.checked })}
        />
        <span>Use the Copilot key (Win+Shift+F23) to summon the overlay</span>
      </label>
      <small className="muted">
        If your Copilot key doesn&apos;t work, remap it to your hotkey with PowerToys Keyboard
        Manager.
      </small>

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

      <label className="check">
        <input
          type="checkbox"
          checked={autostart}
          onChange={async (e) => {
            await ipc.setAutostart(e.target.checked);
            setAutostart(e.target.checked);
          }}
        />
        <span>Start Goose Overlay when I sign in</span>
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
