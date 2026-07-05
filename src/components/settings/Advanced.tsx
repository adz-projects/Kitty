import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { EnvVar } from '@/lib/types';

/** Advanced: Ollama env-var helper (HKCU\Environment). Per-provider sampling
    params (temperature / context length) now live in Settings → Providers. */
export function Advanced() {
  const [envVars, setEnvVars] = useState<EnvVar[]>([]);
  const [msg, setMsg] = useState('');

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

      <p className="muted">
        Temperature and context length are now set per provider in Settings → Providers.
      </p>
    </section>
  );
}
