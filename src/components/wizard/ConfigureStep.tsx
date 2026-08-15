import { useState } from 'react';
import { pickFolder } from '@/lib/ipc';
import type { Config } from '@/lib/types';
import { accelerator } from '@/lib/accelerator';

export function ConfigureStep({
  cfg,
  saveCfg,
  onBack,
  onNext,
}: {
  cfg: Config;
  saveCfg: (patch: Partial<Config>) => Promise<void>;
  onBack: () => void;
  onNext: () => void;
}) {
  const [recording, setRecording] = useState(false);

  return (
    <section className="wizard-panel">
      <h1>A couple of settings</h1>

      <label className="field">
        <span>Hotkey to summon Kitty</span>
        <input
          value={recording ? 'Press a shortcut…' : (cfg.hotkeys[0] ?? '')}
          readOnly={recording}
          onChange={() => {}}
          onKeyDown={(e) => {
            if (!recording) return;
            e.preventDefault();
            const acc = accelerator(e);
            if (acc) {
              void saveCfg({ hotkeys: [acc, ...cfg.hotkeys.slice(1)] });
              setRecording(false);
            }
          }}
        />
        <button type="button" onClick={() => setRecording((r) => !r)}>
          {recording ? 'Cancel' : 'Record a different shortcut'}
        </button>
        <small className="muted">Press this any time to open Kitty from anywhere.</small>
      </label>

      <label className="field">
        <span>Where should Kitty save files it creates for you?</span>
        <div className="row">
          <input
            value={cfg.default_context_folder ?? ''}
            placeholder="%USERPROFILE%\Documents\Kitty"
            onChange={(e) => void saveCfg({ default_context_folder: e.target.value || null })}
          />
          <button
            onClick={async () => {
              const d = await pickFolder();
              if (d) await saveCfg({ default_context_folder: d });
            }}
          >
            Browse…
          </button>
        </div>
        <small className="muted">Leave blank to use the default, ~/Documents/Kitty.</small>
      </label>

      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        <button className="primary" onClick={onNext}>
          Next
        </button>
      </div>
    </section>
  );
}
