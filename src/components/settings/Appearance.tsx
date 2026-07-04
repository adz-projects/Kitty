import { useConfigDraft } from './useConfigDraft';

/** Appearance: theme selection now; user CSS themes + background image land in
    Phase 6. */
export function Appearance() {
  const { draft, update, save, saved } = useConfigDraft();
  if (!draft) return <p className="muted">Loading…</p>;

  return (
    <section className="settings-section">
      <h1>Appearance</h1>
      <label className="field">
        <span>Theme</span>
        <select value={draft.theme} onChange={(e) => update({ theme: e.target.value })}>
          <option value="default">Default (light)</option>
          <option value="dark">Dark</option>
        </select>
      </label>
      <label className="check">
        <input
          type="checkbox"
          checked={draft.remember_overlay_position}
          onChange={(e) => update({ remember_overlay_position: e.target.checked })}
        />
        <span>Remember overlay size &amp; position</span>
      </label>
      <p className="muted">Custom CSS themes and a background image arrive in Phase 6.</p>
      <div className="row">
        <button className="primary" onClick={() => void save()}>
          Save
        </button>
        {saved && <span className="muted">Saved.</span>}
      </div>
    </section>
  );
}
