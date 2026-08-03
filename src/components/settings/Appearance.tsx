import { useEffect, useState } from 'react';
import { ipc, pickImage } from '@/lib/ipc';
import { useConfigDraft } from './useConfigDraft';

/** Appearance: theme (built-in + user CSS), background image + dim, overlay prefs. */
export function Appearance() {
  const { draft, update, save, saved, error } = useConfigDraft();
  const [themes, setThemes] = useState<{ builtins: string[]; user: string[] }>({
    builtins: ['default', 'dark'],
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
          <button onClick={() => void ipc.openThemesFolder()}>Open themes folder</button>
          <button onClick={loadThemes}>Refresh</button>
        </div>
        <small className="muted">
          Drop a <code>.css</code> file of custom properties into the themes folder — see
          themes/README.md for the contract.
        </small>
      </label>

      <label className="field">
        <span>Background image</span>
        <div className="row">
          <input
            value={draft.background_image ?? ''}
            placeholder="(none)"
            onChange={(e) => update({ background_image: e.target.value || null })}
          />
          <button
            onClick={async () => {
              const img = await pickImage();
              if (img) update({ background_image: img });
            }}
          >
            Choose…
          </button>
          {draft.background_image && (
            <button onClick={() => update({ background_image: null })}>Clear</button>
          )}
        </div>
      </label>

      <label className="field">
        <span>Background dim ({Math.round((draft.background_dim ?? 0.3) * 100)}%)</span>
        <input
          type="range"
          min={0}
          max={1}
          step={0.05}
          value={draft.background_dim ?? 0.3}
          onChange={(e) => update({ background_dim: Number(e.target.value) })}
        />
      </label>

      <label className="field">
        <span>Background fit</span>
        <select
          value={draft.background_size ?? 'cover'}
          onChange={(e) =>
            update({ background_size: e.target.value as typeof draft.background_size })
          }
        >
          <option value="cover">Fill</option>
          <option value="contain">Fit</option>
          <option value="stretch">Stretch</option>
          <option value="center">Center</option>
        </select>
      </label>

      <label className="field">
        <span>
          Background position — horizontal ({Math.round(draft.background_position_x ?? 50)}%)
        </span>
        <input
          type="range"
          min={0}
          max={100}
          step={1}
          value={draft.background_position_x ?? 50}
          onChange={(e) => update({ background_position_x: Number(e.target.value) })}
        />
      </label>

      <label className="field">
        <span>
          Background position — vertical ({Math.round(draft.background_position_y ?? 50)}%)
        </span>
        <input
          type="range"
          min={0}
          max={100}
          step={1}
          value={draft.background_position_y ?? 50}
          onChange={(e) => update({ background_position_y: Number(e.target.value) })}
        />
      </label>

      <label className="check">
        <input
          type="checkbox"
          checked={draft.remember_overlay_position}
          onChange={(e) => update({ remember_overlay_position: e.target.checked })}
        />
        <span>Remember overlay size &amp; position</span>
      </label>

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
