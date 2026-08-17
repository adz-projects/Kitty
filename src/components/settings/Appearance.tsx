import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { SYSTEM_THEME } from '@/lib/theme';
import { useConfigDraft } from './useConfigDraft';
import { isAndroid } from '@/lib/platform';

/** Appearance: theme (built-in + user CSS), and — desktop only — the
    "remember overlay size & position" toggle. */
export function Appearance() {
  const { draft, update, save, saved, error } = useConfigDraft();
  const [themes, setThemes] = useState<{ builtins: string[]; user: string[] }>({
    builtins: ['light', 'dark'],
    user: [],
  });

  const loadThemes = () => void ipc.listThemes().then(setThemes);
  useEffect(loadThemes, []);

  if (!draft) return <p className="muted">Loading…</p>;

  return (
    <section className="settings-section">
      <h1>Appearance</h1>

      <label className="field">
        <span>Theme</span>
        <div className="row">
          <select value={draft.theme} onChange={(e) => update({ theme: e.target.value })}>
            {/* Outside the built-in group because it isn't a stylesheet — it
                resolves to `light` or `dark` from the OS preference (D16). */}
            <option value={SYSTEM_THEME}>Match system</option>
            <optgroup label="Built-in">
              {themes.builtins.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </optgroup>
            {themes.user.length > 0 && (
              <optgroup label="Custom (themes folder)">
                {themes.user.map((t) => (
                  <option key={t} value={t}>
                    {t}
                  </option>
                ))}
              </optgroup>
            )}
          </select>
          {/* Both act on the themes folder — Android has no accessible
              filesystem to drop a custom theme .css into in the first
              place, so neither button does anything useful there. */}
          {!isAndroid() && (
            <>
              <button onClick={() => void ipc.openThemesFolder()}>Open themes folder</button>
              <button onClick={loadThemes}>Refresh</button>
            </>
          )}
        </div>
        <small className="muted">
          Drop a <code>.css</code> file of custom properties into the themes folder — see
          themes/README.md for the contract.
        </small>
      </label>

      {/* Desktop-only — there is no overlay window on Android to remember
          anything about. */}
      {!isAndroid() && (
        <label className="check">
          <input
            type="checkbox"
            checked={draft.remember_overlay_position}
            onChange={(e) => update({ remember_overlay_position: e.target.checked })}
          />
          <span>Remember overlay size &amp; position</span>
        </label>
      )}

      <div className="row">
        <button className="primary" onClick={() => void save()}>
          Save &amp; apply
        </button>
        {saved && <span className="muted">Saved.</span>}
        {error && <span className="error">Couldn't save: {error}</span>}
      </div>
    </section>
  );
}
