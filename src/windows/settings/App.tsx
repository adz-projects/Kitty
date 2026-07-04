import { useEffect, useState } from 'react';
import { useSettingsStore } from '@/stores/settingsStore';
import type { Config } from '@/lib/types';

/** Minimal settings for Phase 0/1: enough to prove config round-trips and the
    hotkey re-registers. The full sectioned IA (Providers, Ollama, Appearance,
    Setup & Repair, deep links) lands in Phase 5. */
export function App() {
  const config = useSettingsStore((s) => s.config);
  const load = useSettingsStore((s) => s.load);
  const save = useSettingsStore((s) => s.save);
  const [draft, setDraft] = useState<Config | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    void load();
  }, [load]);
  useEffect(() => {
    if (config) setDraft(config);
  }, [config]);

  if (!draft) return <div className="window-root">Loading…</div>;

  const update = (patch: Partial<Config>) => {
    setDraft({ ...draft, ...patch });
    setSaved(false);
  };

  return (
    <div className="window-root">
      <h1 style={{ fontSize: 20, marginTop: 0 }}>Settings</h1>
      <p className="muted">General settings. More sections arrive in Phase 5.</p>

      <div style={{ display: 'flex', flexDirection: 'column', gap: 16, maxWidth: 480 }}>
        <label style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <span>Toggle hotkey</span>
          <input
            value={draft.hotkey}
            onChange={(e) => update({ hotkey: e.target.value })}
            placeholder="Alt+Space"
          />
        </label>

        <label style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <span>Ollama endpoint</span>
          <input
            value={draft.ollama_base_url}
            onChange={(e) => update({ ollama_base_url: e.target.value })}
          />
        </label>

        <label style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <span>Theme</span>
          <select value={draft.theme} onChange={(e) => update({ theme: e.target.value })}>
            <option value="default">Default</option>
            <option value="dark">Dark</option>
          </select>
        </label>

        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <button
            className="primary"
            onClick={async () => {
              await save(draft);
              setSaved(true);
            }}
          >
            Save
          </button>
          {saved && <span className="muted">Saved.</span>}
        </div>
      </div>
    </div>
  );
}
