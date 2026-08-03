import { useEffect, useState } from 'react';
import { useConfigDraft } from './useConfigDraft';
import { ipc, pickFolder } from '@/lib/ipc';
import { accelerator } from '@/lib/accelerator';
import { Modal } from '@/components/shared/Modal';

/** General settings backed by app config. Approval mode is per-session (see
    the chat mode badge) rather than living here. */
export function General() {
  const { draft, update, save, saved, error } = useConfigDraft();
  // Index of the hotkey row currently capturing a shortcut, or null.
  const [recording, setRecording] = useState<number | null>(null);
  const [recordingClipboard, setRecordingClipboard] = useState(false);
  const [recordingOpenWindow, setRecordingOpenWindow] = useState(false);
  const [autostart, setAutostart] = useState(false);
  const [autostartError, setAutostartError] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [sessionCount, setSessionCount] = useState<number | null>(null);
  const [clearing, setClearing] = useState(false);
  const [clearError, setClearError] = useState<string | null>(null);

  useEffect(() => {
    void ipc.getAutostart().then(setAutostart);
  }, []);

  const openConfirmClear = async () => {
    setClearError(null);
    setConfirmClear(true);
    try {
      setSessionCount((await ipc.listSessions()).length);
    } catch {
      setSessionCount(null);
    }
  };

  if (!draft) return <p className="muted">Loading…</p>;

  return (
    <section className="settings-section">
      <h1>General</h1>

      <label className="field">
        <span>Chats folder</span>
        <div className="row">
          <input
            value={draft.default_context_folder ?? ''}
            placeholder="%USERPROFILE%\\Documents\\Kitty"
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
        <small className="muted">
          Each chat gets its own working directory at{' '}
          <code>&lt;this&gt;/chats/&lt;chat-id&gt;</code>— that&apos;s where files a model creates
          for you are saved. Leave blank for the default, <code>~/Documents/Kitty</code>.
        </small>
      </label>

      <label className="field">
        <span>Ollama endpoint</span>
        <input
          value={draft.ollama_base_url}
          onChange={(e) => update({ ollama_base_url: e.target.value })}
        />
      </label>

      <div className="field">
        <span>Toggle hotkeys</span>
        {draft.hotkeys.map((hk, i) => (
          <div className="row" key={i}>
            <input
              value={recording === i ? 'Press a shortcut…' : hk}
              readOnly={recording === i}
              onChange={(e) =>
                update({ hotkeys: draft.hotkeys.map((h, j) => (j === i ? e.target.value : h)) })
              }
              onKeyDown={(e) => {
                if (recording !== i) return;
                e.preventDefault();
                const acc = accelerator(e);
                if (acc) {
                  update({ hotkeys: draft.hotkeys.map((h, j) => (j === i ? acc : h)) });
                  setRecording(null);
                }
              }}
            />
            <button onClick={() => setRecording((r) => (r === i ? null : i))}>
              {recording === i ? 'Cancel' : 'Record'}
            </button>
            <button
              onClick={() => {
                update({ hotkeys: draft.hotkeys.filter((_, j) => j !== i) });
                setRecording(null);
              }}
              disabled={draft.hotkeys.length <= 1}
              title={draft.hotkeys.length <= 1 ? 'Keep at least one hotkey' : 'Remove'}
            >
              ✕
            </button>
          </div>
        ))}
        <button
          className="link"
          onClick={() => {
            update({ hotkeys: [...draft.hotkeys, 'Alt+Space'] });
            setRecording(draft.hotkeys.length);
          }}
        >
          + Add another hotkey
        </button>
        <small className="muted">Save to apply. Any of them summons the overlay.</small>
      </div>

      <div className="field">
        <span>Clipboard hotkey</span>
        <div className="row">
          <input
            value={recordingClipboard ? 'Press a shortcut…' : (draft.clipboard_hotkey ?? '')}
            readOnly={recordingClipboard}
            placeholder="Not set"
            onChange={() => {}}
            onKeyDown={(e) => {
              if (!recordingClipboard) return;
              e.preventDefault();
              const acc = accelerator(e);
              if (acc) {
                update({ clipboard_hotkey: acc });
                setRecordingClipboard(false);
              }
            }}
          />
          <button onClick={() => setRecordingClipboard((r) => !r)}>
            {recordingClipboard ? 'Cancel' : 'Record'}
          </button>
          <button
            onClick={() => {
              update({ clipboard_hotkey: null });
              setRecordingClipboard(false);
            }}
            disabled={!draft.clipboard_hotkey}
            title="Clear"
          >
            ✕
          </button>
        </div>
        <small className="muted">
          Save to apply. Summons the overlay with the current clipboard (text or image)
          pre-attached.
        </small>
      </div>

      <div className="field">
        <span>Open new chat window hotkey</span>
        <div className="row">
          <input
            value={recordingOpenWindow ? 'Press a shortcut…' : (draft.open_window_hotkey ?? '')}
            readOnly={recordingOpenWindow}
            placeholder="Not set"
            onChange={() => {}}
            onKeyDown={(e) => {
              if (!recordingOpenWindow) return;
              e.preventDefault();
              const acc = accelerator(e);
              if (acc) {
                update({ open_window_hotkey: acc });
                setRecordingOpenWindow(false);
              }
            }}
          />
          <button onClick={() => setRecordingOpenWindow((r) => !r)}>
            {recordingOpenWindow ? 'Cancel' : 'Record'}
          </button>
          <button
            onClick={() => {
              update({ open_window_hotkey: null });
              setRecordingOpenWindow(false);
            }}
            disabled={!draft.open_window_hotkey}
            title="Clear"
          >
            ✕
          </button>
        </div>
        <small className="muted">
          Save to apply. Always opens a brand-new chat window with a fresh session — never reuses an
          existing one.
        </small>
      </div>

      <label className="check">
        <input
          type="checkbox"
          checked={autostart}
          onChange={(e) => {
            // Read `checked` *before* awaiting, not after — confirmed real
            // bug: this is a controlled input, so React restores the DOM
            // checkbox to match `checked={autostart}` (still false) as soon
            // as the handler yields. Reading `e.target.checked` after the
            // IPC round-trip therefore read back the restored `false` and
            // set state to it, so the registry key was written correctly but
            // the checkbox snapped straight back to off — indistinguishable
            // from "the toggle doesn't work", and a second click then wrote
            // `false` and really did undo it.
            const next = e.target.checked;
            void ipc
              .setAutostart(next)
              .then(() => {
                setAutostart(next);
                setAutostartError(null);
              })
              .catch((err) => setAutostartError(String(err)));
          }}
        />
        <span>Start Kitty when I sign in</span>
      </label>
      {autostartError && <div className="chat-error">{autostartError}</div>}

      <p className="muted">
        Approval mode is per session — change it from the shield badge next to the composer.
      </p>

      <div className="field">
        <span>Danger zone</span>
        <button onClick={() => void openConfirmClear()}>Clear all chat history</button>
        <small className="muted">
          Permanently deletes every conversation and its working-directory files.
        </small>
      </div>

      <div className="row">
        <button className="primary" onClick={() => void save()}>
          Save
        </button>
        {saved && <span className="muted">Saved.</span>}
        {error && <span className="error">Couldn't save: {error}</span>}
      </div>

      {confirmClear && (
        <Modal title="Clear all chat history?">
          <p>
            This permanently deletes {sessionCount ?? 'all'} conversation(s) and their
            working-directory files. This cannot be undone.
          </p>
          {clearError && <div className="chat-error">{clearError}</div>}
          <div className="row">
            <button
              className="primary"
              disabled={clearing}
              onClick={async () => {
                setClearing(true);
                setClearError(null);
                try {
                  await ipc.clearAllSessions();
                  setConfirmClear(false);
                } catch (e) {
                  setClearError(String(e));
                } finally {
                  setClearing(false);
                }
              }}
            >
              {clearing ? 'Deleting…' : 'Yes, delete everything'}
            </button>
            <button onClick={() => setConfirmClear(false)} disabled={clearing}>
              Cancel
            </button>
          </div>
        </Modal>
      )}
    </section>
  );
}
