import { useEffect, useRef, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useConfigDraft } from './useConfigDraft';
import type { EnvVar, LogEntry, ProviderView } from '@/lib/types';

// How often to re-fetch the error log while its disclosure is open — there's
// no push event for new entries (kept simple, matching this being a
// diagnostic-only view rather than a live-critical one), so a short poll
// keeps it reasonably current without the user needing to leave and reopen
// Settings.
const LOG_POLL_MS = 5000;

/** Advanced: Ollama env-var helper (HKCU\Environment), plus the two
    infrequently-touched General settings relocated here in the settings IA
    overhaul (context strategy, strict remote mode) — trimmed off General to
    keep that page to the essentials. Per-provider sampling params
    (temperature / context length) live in Settings → Providers. */
export function Advanced() {
  const [envVars, setEnvVars] = useState<EnvVar[]>([]);
  const [msg, setMsg] = useState('');
  const { draft, update, save, saved, error: saveError } = useConfigDraft();
  const [envOpen, setEnvOpen] = useState(true);
  const [tokenMgmtOpen, setTokenMgmtOpen] = useState(false);
  const [enablingOllama, setEnablingOllama] = useState(false);
  const [ollamaMsg, setOllamaMsg] = useState('');
  const [logOpen, setLogOpen] = useState(false);
  const [logEntries, setLogEntries] = useState<LogEntry[]>([]);
  const [logError, setLogError] = useState('');
  const logPollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const [providers, setProviders] = useState<ProviderView[]>([]);

  const loadProviders = () =>
    void ipc
      .listProviders()
      .then(setProviders)
      .catch(() => setProviders([]));
  useEffect(loadProviders, []);

  // The active provider's own `context_length` (Settings → Providers →
  // Advanced) wins over the global `max_context_tokens` below as the per-chat
  // budget — the global value only applies to providers with no override set.
  const activeProvider = providers.find((p) => p.active) ?? null;
  const providerCtxLen = activeProvider?.context_length ?? null;

  const load = () =>
    void ipc
      .readOllamaEnv()
      .then(setEnvVars)
      .catch((e) => setMsg(`Couldn't read Ollama env vars: ${String(e)}`));
  useEffect(load, []);

  const loadLogEntries = () =>
    void ipc
      .listLogEntries()
      .then(setLogEntries)
      .catch((e) => setLogError(String(e)));

  // Only poll while the disclosure is actually open — no point fetching a log
  // nobody's looking at.
  useEffect(() => {
    if (!logOpen) {
      if (logPollRef.current) clearInterval(logPollRef.current);
      logPollRef.current = null;
      return;
    }
    loadLogEntries();
    logPollRef.current = setInterval(loadLogEntries, LOG_POLL_MS);
    return () => {
      if (logPollRef.current) clearInterval(logPollRef.current);
      logPollRef.current = null;
    };
  }, [logOpen]);

  const clearLog = async () => {
    try {
      await ipc.clearLogEntries();
      setLogEntries([]);
    } catch (e) {
      setLogError(String(e));
    }
  };

  // Writes straight through ipc.setConfig (not update()+save(), which would
  // race — save() closes over the pre-update draft) so the flag flips
  // immediately, matching how the wizard's own saveCfg behaves.
  const applyOllamaEnabled = async (enabled: boolean) => {
    if (!draft) return;
    const next = { ...draft, ollama_enabled: enabled };
    await ipc.setConfig(next);
    update({ ollama_enabled: enabled });
  };

  const enableAndInstallOllama = async () => {
    setEnablingOllama(true);
    setOllamaMsg('');
    try {
      const det = await ipc.detectDependencies();
      if (!det.ollama.installed) {
        await ipc.installDependency('ollama');
        setOllamaMsg('Installer launched — finish that window, then check back here.');
      } else {
        setOllamaMsg('Ollama is already installed.');
      }
      await applyOllamaEnabled(true);
    } catch (e) {
      setOllamaMsg(String(e));
    } finally {
      setEnablingOllama(false);
    }
  };

  const setVar = async (name: string, value: string) => {
    try {
      await ipc.setOllamaEnv(name, value || null);
      setMsg(`${name} saved (restart Ollama to apply).`);
      load();
    } catch (e) {
      setMsg(`Couldn't save ${name}: ${String(e)}`);
    }
  };

  return (
    <section className="settings-section">
      <h1>Advanced</h1>

      {draft && (
        <>
          <div className="field">
            <span>Local inference</span>
            {draft.ollama_enabled ? (
              <>
                <p className="muted" style={{ margin: 0 }}>
                  On — see Settings → Ollama Models to manage installed models.
                </p>
                <button className="link" onClick={() => void applyOllamaEnabled(false)}>
                  Turn off local inference
                </button>
              </>
            ) : (
              <>
                <p className="muted" style={{ margin: 0 }}>
                  Off — you picked an API key during setup. You can turn this on any time.
                </p>
                <button disabled={enablingOllama} onClick={() => void enableAndInstallOllama()}>
                  {enablingOllama ? 'Working…' : 'Enable & install Ollama'}
                </button>
              </>
            )}
            {ollamaMsg && <small className="muted">{ollamaMsg}</small>}
          </div>

          <label className="check">
            <input
              type="checkbox"
              checked={draft.strict_remote_mode}
              onChange={(e) => update({ strict_remote_mode: e.target.checked })}
            />
            <span>Strict mode: disable file/folder drop while a remote provider is active</span>
          </label>

          <div className="field">
            <span>Background context summarization</span>
            <p className="muted" style={{ margin: 0 }}>
              Folds older conversation history into a running summary so long agentic sessions
              don&apos;t run out of context. Uses a small local model via Ollama; changes need a
              backend restart to take effect.
            </p>
            <label className="check">
              <input
                type="checkbox"
                checked={draft.summarizer.enabled}
                onChange={(e) =>
                  update({ summarizer: { ...draft.summarizer, enabled: e.target.checked } })
                }
              />
              <span>Enabled</span>
            </label>
            {draft.summarizer.enabled && (
              <>
                <label className="field">
                  <span>Summarizer model (Ollama tag)</span>
                  <input
                    value={draft.summarizer.model}
                    onChange={(e) =>
                      update({ summarizer: { ...draft.summarizer, model: e.target.value } })
                    }
                  />
                </label>
                <label className="field">
                  <span>VRAM retention (Ollama keep_alive)</span>
                  <select
                    value={draft.summarizer.keep_alive}
                    onChange={(e) =>
                      update({ summarizer: { ...draft.summarizer, keep_alive: e.target.value } })
                    }
                  >
                    <option value="0">Unload immediately after each pass</option>
                    <option value="5m">Keep loaded for 5 minutes</option>
                    <option value="-1">Keep loaded permanently</option>
                  </select>
                </label>
              </>
            )}

            <button
              type="button"
              className="disclosure-toggle"
              onClick={() => setTokenMgmtOpen((o) => !o)}
            >
              {tokenMgmtOpen ? '▾' : '▸'} <strong>Token management</strong>
            </button>
            {tokenMgmtOpen && (
              <>
                <label className="field">
                  <span>Max context tokens</span>
                  <input
                    type="number"
                    min={8192}
                    step={1024}
                    value={draft.token_management.max_context_tokens}
                    onChange={(e) => {
                      // `Number('') === 0`, which would violate the declared
                      // `min` and get persisted as an invalid 0 — an emptied
                      // field keeps the previous value instead (the type here
                      // is a non-nullable number, so there's no null to write).
                      const raw = e.target.value;
                      if (raw === '') return;
                      const numeric = Number(raw);
                      if (!Number.isFinite(numeric)) return;
                      update({
                        token_management: {
                          ...draft.token_management,
                          max_context_tokens: numeric,
                        },
                      });
                    }}
                  />
                  <small className="muted">
                    BigTiny&apos;s context window size. Must match your active model&apos;s
                    capability.
                  </small>
                  {providerCtxLen != null ? (
                    <p className="muted" style={{ margin: 0 }}>
                      Active provider <strong>{activeProvider?.name ?? 'provider'}</strong>{' '}
                      overrides this to{' '}
                      <strong>
                        {providerCtxLen.toLocaleString()} tokens (effective this chat)
                      </strong>{' '}
                      via Settings → Providers. This global value is only the fallback for providers
                      without an override.
                    </p>
                  ) : (
                    <p className="muted" style={{ margin: 0 }}>
                      No context-length override set on the active provider — this global value is
                      the effective per-chat budget. Set one in Settings → Providers to scope it per
                      provider.
                    </p>
                  )}
                  <div className="row">
                    <button
                      type="button"
                      disabled={providerCtxLen == null}
                      onClick={() =>
                        update({
                          token_management: {
                            ...draft!.token_management,
                            max_context_tokens: providerCtxLen!,
                          },
                        })
                      }
                    >
                      Match active provider ({providerCtxLen?.toLocaleString() ?? '—'})
                    </button>
                  </div>
                </label>
                <label className="field">
                  <span>Max live tail tokens</span>
                  <input
                    type="number"
                    min={1024}
                    step={1024}
                    value={draft.token_management.max_live_tail_tokens}
                    onChange={(e) => {
                      // Same empty-field guard as max_context_tokens: don't
                      // persist `Number('')` === 0 against the `min` of 1024.
                      const raw = e.target.value;
                      if (raw === '') return;
                      const numeric = Number(raw);
                      if (!Number.isFinite(numeric)) return;
                      update({
                        token_management: {
                          ...draft.token_management,
                          max_live_tail_tokens: numeric,
                        },
                      });
                    }}
                  />
                  <small className="muted">
                    Per-turn budget for the live conversation tail. Lower = more aggressive
                    compaction.
                  </small>
                </label>
                <label className="field">
                  <span>Code block head lines</span>
                  <input
                    type="number"
                    min={0}
                    max={50}
                    value={draft.token_management.message_mask_head_lines}
                    onChange={(e) =>
                      update({
                        token_management: {
                          ...draft.token_management,
                          message_mask_head_lines: Number(e.target.value),
                        },
                      })
                    }
                  />
                </label>
                <label className="field">
                  <span>Code block tail lines</span>
                  <input
                    type="number"
                    min={0}
                    max={50}
                    value={draft.token_management.message_mask_tail_lines}
                    onChange={(e) =>
                      update({
                        token_management: {
                          ...draft.token_management,
                          message_mask_tail_lines: Number(e.target.value),
                        },
                      })
                    }
                  />
                  <small className="muted">
                    Lines kept at head/tail of code blocks in older messages. Set to 0 to disable
                    masking.
                  </small>
                </label>
                <p className="muted" style={{ margin: 0 }}>
                  Token management changes require a backend restart.
                </p>
              </>
            )}
          </div>

          <div className="row">
            <button onClick={() => void ipc.restartBackend()}>Restart backend now</button>
          </div>

          <div className="row">
            <button className="primary" onClick={() => void save()}>
              Save
            </button>
            {saved && <span className="muted">Saved.</span>}
            {saveError && <span className="error">Couldn't save: {saveError}</span>}
          </div>
        </>
      )}

      <button type="button" className="disclosure-toggle" onClick={() => setEnvOpen((o) => !o)}>
        {envOpen ? '▾' : '▸'} <strong>Ollama environment variables</strong>
      </button>
      {/* Explicit conditional render, not native <details> collapse — this
          WebView2/Chromium build doesn't actually hide non-open <details>
          content, so visibility can't be left to CSS (see Providers.tsx's
          equivalent comment for the full finding). */}
      {envOpen && (
        <div>
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
        </div>
      )}

      <p className="muted">
        Temperature and context length are now set per provider in Settings → Providers.
      </p>

      <button type="button" className="disclosure-toggle" onClick={() => setLogOpen((o) => !o)}>
        {logOpen ? '▾' : '▸'} <strong>Error log</strong>
      </button>
      {logOpen && (
        <div>
          <p className="muted">
            Warnings and errors captured from Kitty&apos;s own background processes (goosed
            connection issues, health checks, provider/config problems) — useful for reporting a
            bug. This doesn&apos;t include anything the model itself said, only Kitty&apos;s own
            internal diagnostics.
          </p>
          {logError && <div className="chat-error">{logError}</div>}
          {logEntries.length === 0 && !logError && (
            <p className="muted">No warnings or errors recorded.</p>
          )}
          <div className="log-entries">
            {logEntries.map((entry, i) => (
              <div className={`log-entry log-entry-${entry.level.toLowerCase()}`} key={i}>
                <span className="log-entry-head">
                  <span className="log-entry-level">{entry.level}</span>
                  <span className="muted log-entry-time">
                    {new Date(entry.timestamp).toLocaleString()}
                  </span>
                  <span className="muted log-entry-target">{entry.target}</span>
                </span>
                <span className="log-entry-message">{entry.message}</span>
              </div>
            ))}
          </div>
          <div className="row">
            <button onClick={loadLogEntries}>Refresh</button>
            <button onClick={() => void clearLog()} disabled={logEntries.length === 0}>
              Clear
            </button>
          </div>
        </div>
      )}
    </section>
  );
}
