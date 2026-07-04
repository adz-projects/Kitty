import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { EnvVar } from '@/lib/types';
import { useConfigDraft } from './useConfigDraft';

/** Advanced: Ollama env-var helper (HKCU\Environment) + model params. */
export function Advanced() {
  const [envVars, setEnvVars] = useState<EnvVar[]>([]);
  const [msg, setMsg] = useState('');
  const { draft, update, save, saved } = useConfigDraft();

  const load = () => void ipc.readOllamaEnv().then(setEnvVars);
  useEffect(load, []);

  const setVar = async (name: string, value: string) => {
    await ipc.setOllamaEnv(name, value || null);
    setMsg(`${name} saved (restart Ollama to apply).`);
    load();
  };

  return (
    <section className="settings-section">
      <h1>Advanced</h1>

      <details open>
        <summary>
          <strong>Ollama environment variables</strong>
        </summary>
        <p className="muted">
          Stored in your user environment (HKCU). A running Ollama must be restarted to pick up
          changes.
        </p>
        {envVars.map((v) => (
          <label className="field" key={v.name}>
            <span>{v.name}</span>
            <div className="row">
              <input
                defaultValue={v.value ?? ''}
                placeholder="(unset)"
                onBlur={(e) => void setVar(v.name, e.target.value)}
              />
            </div>
          </label>
        ))}
        <div className="row">
          <button
            onClick={async () => {
              try {
                await ipc.restartOllama();
                setMsg('Ollama restarted.');
              } catch (e) {
                setMsg(String(e));
              }
            }}
          >
            Restart Ollama now
          </button>
          {msg && <span className="muted">{msg}</span>}
        </div>
      </details>

      <details>
        <summary>
          <strong>Model parameters</strong>
        </summary>
        {draft && (
          <>
            <label className="field">
              <span>Temperature</span>
              <input
                type="number"
                step="0.1"
                value={draft.model_params.temperature ?? ''}
                onChange={(e) =>
                  update({
                    model_params: {
                      ...draft.model_params,
                      temperature: e.target.value ? Number(e.target.value) : null,
                    },
                  })
                }
              />
            </label>
            <label className="field">
              <span>Context length</span>
              <input
                type="number"
                value={draft.model_params.context_length ?? ''}
                onChange={(e) =>
                  update({
                    model_params: {
                      ...draft.model_params,
                      context_length: e.target.value ? Number(e.target.value) : null,
                    },
                  })
                }
              />
            </label>
            <div className="row">
              <button className="primary" onClick={() => void save()}>
                Save (applies on next Goose restart)
              </button>
              {saved && <span className="muted">Saved.</span>}
            </div>
          </>
        )}
      </details>
    </section>
  );
}
