import { useEffect, useRef, useState } from 'react';
import { useConfigDraft } from './useConfigDraft';
import { isAndroid } from '@/lib/platform';
import { ipc, pickFolder } from '@/lib/ipc';
import { accelerator } from '@/lib/accelerator';

/** General settings backed by app config. Approval mode is per-session (see
    the chat mode badge) rather than living here. */
export function General() {
  const { draft, update, save, saved, error } = useConfigDraft();
  const [recordingToggle, setRecordingToggle] = useState(false);
  const [recordingClipboard, setRecordingClipboard] = useState(false);
  const [recordingOpenWindow, setRecordingOpenWindow] = useState(false);
  const [autostart, setAutostart] = useState(false);
  const [autostartError, setAutostartError] = useState<string | null>(null);
  // One ref per row, so entering record mode can focus the target input —
  // otherwise the recorded keystrokes were swallowed (nothing was focused).
  const toggleRef = useRef<HTMLInputElement>(null);
  const clipboardRef = useRef<HTMLInputElement>(null);
  const openWindowRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (recordingToggle) toggleRef.current?.focus();
  }, [recordingToggle]);
  useEffect(() => {
    if (recordingClipboard) clipboardRef.current?.focus();
  }, [recordingClipboard]);
  useEffect(() => {
    if (recordingOpenWindow) openWindowRef.current?.focus();
  }, [recordingOpenWindow]);

  useEffect(() => {
    // `get_autostart` is the HKCU Run key and is `#[cfg(desktop)]`-gated out
    // of the Android handler list entirely, so invoking it there isn't a
    // no-op — it rejects with "command not found". Skipped rather than
    // caught, so a real failure on desktop still surfaces.
    if (isAndroid()) return;
    void ipc.getAutostart().then(setAutostart);
  }, []);

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

      {/* Desktop-only from here down. Neither describes anything Android can
          do: there is no overlay to summon and no OS-wide shortcut
          registration (D9/D23), and no login session to start with.
          Rendering them would be offering settings that cannot take effect. */}
      {!isAndroid() && (
        <>
          <div className="field">
            <span>Hotkeys</span>
            <small className="muted">
              Save to apply. The toggle hotkey summons the overlay; the others are optional.
            </small>
            <table className="settings-table hotkey-table">
              <thead>
                <tr>
                  <th>Action</th>
                  <th>Hotkey</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                <tr>
                  <td>Toggle overlay</td>
                  <td>
                    <input
                      ref={toggleRef}
                      value={recordingToggle ? 'Press a shortcut…' : (draft.hotkeys[0] ?? '')}
                      readOnly={recordingToggle}
                      placeholder="Not set"
                      onChange={() => {}}
                      onKeyDown={(e) => {
                        if (!recordingToggle) return;
                        e.preventDefault();
                        const acc = accelerator(e);
                        if (acc) {
                          update({ hotkeys: [acc] });
                          setRecordingToggle(false);
                        }
                      }}
                    />
                  </td>
                  <td>
                    <div className="row">
                      <button onClick={() => setRecordingToggle((r) => !r)}>
                        {recordingToggle ? 'Cancel' : 'Record'}
                      </button>
                    </div>
                  </td>
                </tr>
                <tr>
                  <td>Clipboard</td>
                  <td>
                    <input
                      ref={clipboardRef}
                      value={
                        recordingClipboard ? 'Press a shortcut…' : (draft.clipboard_hotkey ?? '')
                      }
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
                  </td>
                  <td>
                    <div className="row">
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
                  </td>
                </tr>
                <tr>
                  <td>Open new chat window</td>
                  <td>
                    <input
                      ref={openWindowRef}
                      value={
                        recordingOpenWindow ? 'Press a shortcut…' : (draft.open_window_hotkey ?? '')
                      }
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
                  </td>
                  <td>
                    <div className="row">
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
                  </td>
                </tr>
              </tbody>
            </table>
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
        </>
      )}

      <p className="muted">
        Approval mode is per session — change it from the shield badge next to the composer.
      </p>

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
