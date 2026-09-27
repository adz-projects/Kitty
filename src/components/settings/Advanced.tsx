import { useEffect, useRef, useState } from 'react';
import { ipc, pickSavePath } from '@/lib/ipc';
import { useConfigDraft } from './useConfigDraft';
import { ClearChatHistory } from './ClearChatHistory';
import { useStackStore } from '@/stores/stackStore';
import { useRouteStore } from '@/stores/routeStore';
import { isAndroid } from '@/lib/platform';
import { confirmDialog } from '@/components/shared/ConfirmDialog';
import type { EngineInfo, LogEntry, MemoryStats, RestartBlocker } from '@/lib/types';

type SummarizerStatus = Awaited<ReturnType<typeof ipc.getSummarizerStatus>>;

// How often to re-fetch the error log while its disclosure is open — there's
// no push event for new entries (kept simple, matching this being a
// diagnostic-only view rather than a live-critical one), so a short poll
// keeps it reasonably current without the user needing to leave and reopen
// Settings.
const LOG_POLL_MS = 5000;

// How often to re-poll the daemon's global pre-flight memory recall counters
// for the "% of prompts" readout below. Live-ish, not push-driven, matching
// the log poll's simplicity.
const MEMORY_POLL_MS = 5000;

/** Advanced: the infrequently-touched settings, kept off General so that page
    stays to the essentials. Per-provider sampling params (temperature /
    context length) live in Settings → Providers; the helper models live in
    Settings → Helper Models. */
export function Advanced() {
  const { draft, update, save, saved, error: saveError } = useConfigDraft();
  const [tokenMgmtOpen, setTokenMgmtOpen] = useState(false);
  const [memoryOpen, setMemoryOpen] = useState(false);
  const [logOpen, setLogOpen] = useState(false);
  const [logEntries, setLogEntries] = useState<LogEntry[]>([]);
  const [logError, setLogError] = useState('');
  const logPollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const memoryPollRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const [memoryStats, setMemoryStats] = useState<MemoryStats | null>(null);
  const [memoryStatsError, setMemoryStatsError] = useState('');

  // Setup & Repair (merged in — release-fixes item 21): no reason for a
  // one-line stack status plus three buttons to be its own nav tab.
  const stackStatus = useStackStore((s) => s.status);
  const initStack = useStackStore((s) => s.init);
  const [repairMsg, setRepairMsg] = useState('');
  useEffect(() => void initStack(), [initStack]);
  const goto = useRouteStore((s) => s.goto);
  const android = isAndroid();

  // Who started the engine, and so whose settings it is running with (#61).
  const [engine, setEngine] = useState<EngineInfo | null>(null);
  const [blockedBy, setBlockedBy] = useState<RestartBlocker[]>([]);
  // Which summarizer compaction actually uses (#71).
  const [summarizer, setSummarizer] = useState<SummarizerStatus | null>(null);
  const [logCopied, setLogCopied] = useState(false);
  const [handoffReset, setHandoffReset] = useState(false);
  useEffect(() => {
    void ipc
      .getEngineInfo()
      .then(setEngine)
      .catch(() => {});
    void ipc
      .getSummarizerStatus()
      .then(setSummarizer)
      .catch(() => {});
  }, [saved]);

  const restartEngine = async (force: boolean) => {
    if (force) {
      const ok = await confirmDialog({
        title: 'Restart the engine anyway?',
        message: `${blockedBy.map((b) => b.display_name).join(', ')} will lose the engine for a few seconds, and anything they are in the middle of stops.`,
        confirmLabel: 'Restart anyway',
        danger: true,
      });
      if (!ok) return;
    }
    await runRepairAction('Restarting Kitty engine', async () => {
      const outcome = await ipc.restartBackend(force);
      setBlockedBy(outcome.blocked_by);
      if (!outcome.restarted) {
        throw new Error(
          `Not restarted: ${[...new Set(outcome.blocked_by.map((b) => b.display_name))].join(', ')} is using the engine. It restarts on its own once they're done.`
        );
      }
    });
  };

  const copyLog = async () => {
    try {
      await navigator.clipboard.writeText(await ipc.logText());
      setLogCopied(true);
      setTimeout(() => setLogCopied(false), 1500);
    } catch (e) {
      setLogError(String(e));
    }
  };
  const saveLog = async () => {
    const path = await pickSavePath('kitty-errors.log', {
      name: 'Log',
      extensions: ['log', 'txt'],
    });
    if (!path) return;
    try {
      await ipc.saveLogFile(path);
    } catch (e) {
      setLogError(String(e));
    }
  };
  const runRepairAction = async (label: string, fn: () => Promise<void>) => {
    setRepairMsg(`${label}…`);
    try {
      await fn();
      setRepairMsg(`${label} — done.`);
    } catch (e) {
      setRepairMsg(String(e));
    }
  };

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

  const loadMemoryStats = () =>
    void ipc
      .getMemoryStats()
      .then(setMemoryStats)
      .catch((e) => setMemoryStatsError(String(e)));

  // Poll the daemon's global pre-flight memory recall counters only while the
  // disclosure is open, so the "% of prompts" readout stays live — same
  // no-point-fetching-what-people-can't-see rationale as the log poll above
  // (the stats are only consumed while `memoryOpen`, so an unconditional
  // interval would hammer the daemon + re-render this section every 5s even
  // with the panel collapsed).
  useEffect(() => {
    if (!memoryOpen) {
      if (memoryPollRef.current) clearInterval(memoryPollRef.current);
      memoryPollRef.current = null;
      return;
    }
    loadMemoryStats();
    memoryPollRef.current = setInterval(loadMemoryStats, MEMORY_POLL_MS);
    return () => {
      if (memoryPollRef.current) clearInterval(memoryPollRef.current);
      memoryPollRef.current = null;
    };
  }, [memoryOpen]);

  const clearLog = async () => {
    try {
      await ipc.clearLogEntries();
      setLogEntries([]);
    } catch (e) {
      setLogError(String(e));
    }
  };
  return (
    <section className="settings-section">
      <h1>Advanced</h1>

      {draft && (
        <>
          <div className="field">
            <span>Background context summarization</span>
            <p className="muted" style={{ margin: 0 }}>
              Folds older conversation history into a running summary so long sessions don&apos;t
              run out of context.
            </p>
            {/* Desktop only: Android always summarizes with the chat's own
                provider, since no generative model runs on the phone. */}
            {!android && (
              <label className="field">
                <span>Summarize with</span>
                <select
                  value={draft.summarizer.enabled ? 'local' : 'provider'}
                  onChange={(e) =>
                    update({
                      summarizer: { ...draft.summarizer, enabled: e.target.value === 'local' },
                    })
                  }
                >
                  <option value="local">The local model (private, no API cost)</option>
                  <option value="provider">The chat&apos;s own provider</option>
                </select>
              </label>
            )}
            {!android && draft.summarizer.enabled && summarizer && !summarizer.model_installed && (
              <p className="muted" style={{ margin: 0 }}>
                The local summarizer isn&apos;t downloaded, so the chat&apos;s provider summarizes
                until it is.{' '}
                <button
                  className="link"
                  onClick={() => goto('settings', { section: 'local_models' })}
                >
                  Download it in Helper Models
                </button>
              </p>
            )}
            {!android && summarizer && summarizer.effective === 'local' && (
              <p className="muted" style={{ margin: 0 }}>
                Using {summarizer.model}.
              </p>
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
                {/* "Max context tokens", its "Match active provider" button and
                    "Max live tail tokens" used to live here. All three are now
                    derived automatically from the active provider's real window
                    (detected per model, self-correcting when a provider reports
                    its own limit), so there is nothing left for a person to set
                    correctly and a great deal to set wrongly.

                    They were also actively harmful. The live-tail budget is now
                    scaled to the window rather than being a flat 24000 tokens —
                    which on a 36k model reserved two thirds of the context for
                    trailing history and made chats uncompactable within about
                    three turns. A manual override here would let exactly that
                    bug back in, so the field is gone rather than merely
                    defaulted. The daemon config fields remain as the fallback
                    for a provider that advertises no window at all. */}
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
              </>
            )}

            <button
              type="button"
              className="disclosure-toggle"
              onClick={() => setMemoryOpen((o) => !o)}
            >
              {memoryOpen ? '▾' : '▸'} <strong>Pre-flight memory recall</strong>
            </button>
            {memoryOpen && (
              <>
                <p className="muted" style={{ margin: 0 }}>
                  Semantic recall of older turns is injected into each prompt tail so long agentic
                  sessions retain cross-turn context.
                </p>
                <label className="field">
                  <span>Minimum bm25 relevance score</span>
                  <input
                    type="number"
                    step={0.1}
                    placeholder="-2.0 (empty = no gate)"
                    value={draft.memory.bm25_threshold ?? ''}
                    onChange={(e) => {
                      // Empty string ⇔ null (gate off); anything else must be
                      // a finite number to be worth writing.
                      const raw = e.target.value;
                      const numeric = raw === '' ? null : Number(raw);
                      if (raw !== '' && !Number.isFinite(numeric)) return;
                      update({ memory: { ...draft.memory, bm25_threshold: numeric } });
                    }}
                  />
                  <small className="muted">
                    FTS BM25 relevance scores are negative — closer to 0 is more relevant. Set a
                    minimum (e.g. -2.0) to skip weaker matches. Leave empty to disable the gate.
                  </small>
                </label>
                <div className="field">
                  <span>Prompts with injected context</span>
                  {memoryStatsError ? (
                    <p className="error" style={{ margin: 0 }}>
                      {memoryStatsError}
                    </p>
                  ) : memoryStats == null ? (
                    <p className="muted" style={{ margin: 0 }}>
                      Loading…
                    </p>
                  ) : (
                    <p className="muted" style={{ margin: 0 }}>
                      <strong>{memoryStats.injection_rate_pct.toFixed(1)}%</strong> (
                      {memoryStats.injected_prompts} of {memoryStats.total_prompts} prompts, all
                      sessions)
                    </p>
                  )}
                </div>
              </>
            )}
          </div>

          <div className="field">
            <span>Engine and setup</span>
            <p className="muted" style={{ margin: 0 }}>
              Stack status: <strong>{stackStatus.replace(/_/g, ' ')}</strong>
              {engine?.daemon_version ? ` · engine ${engine.daemon_version}` : ''}
            </p>
            {android ? (
              <p className="muted" style={{ margin: 0 }}>
                The engine runs inside Kitty on this phone; settings above apply the next time Kitty
                starts.
              </p>
            ) : (
              engine &&
              !engine.spawned_by_us && (
                <p className="muted" style={{ margin: 0 }}>
                  Another app started the engine Kitty is using, so it is running with that
                  app&apos;s start-up settings. Kitty&apos;s settings above apply after the engine
                  restarts.
                </p>
              )
            )}
            <div className="row">
              {!android && (
                <button onClick={() => void restartEngine(false)}>Restart engine now</button>
              )}
              {!android && blockedBy.length > 0 && (
                <button className="danger" onClick={() => void restartEngine(true)}>
                  Restart anyway
                </button>
              )}
              <button onClick={() => void ipc.openWizard('setup')}>Run first-run wizard</button>
              <button onClick={() => void ipc.openWizard('repair')}>Repair setup</button>
            </div>
            {repairMsg && <p className="muted">{repairMsg}</p>}
          </div>

          <div className="row">
            <button
              className="primary"
              onClick={() => {
                void (async () => {
                  // Saving an engine setting schedules the restart itself
                  // (`engine_restart::schedule`), queued if the engine is
                  // busy; a second request here would only collide with it.
                  await save();
                })();
              }}
            >
              Save
            </button>
            {saved && <span className="muted">Saved.</span>}
            {saveError && <span className="error">Couldn't save: {saveError}</span>}
          </div>
        </>
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
            Warnings and errors captured from Kitty&apos;s own background processes (engine
            connection issues, health checks, provider/config problems) — useful for reporting a
            bug. This doesn&apos;t include anything the model itself said, only Kitty&apos;s own
            internal diagnostics.
          </p>
          {logError && <div className="chat-error">{logError}</div>}
          {logEntries.length === 0 && !logError && (
            <p className="muted">No warnings or errors recorded.</p>
          )}
          <div className="log-entries">
            {logEntries.map((entry) => (
              <div
                className={`log-entry log-entry-${entry.level.toLowerCase()}`}
                // Stable identity, not the array index: this list is
                // re-fetched every 5s while open, and index keys reshuffle
                // row state/DOM whenever a new entry lands at the top.
                key={`${entry.timestamp}:${entry.target}:${entry.message}`}
              >
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
            <button onClick={() => void copyLog()} disabled={logEntries.length === 0}>
              {logCopied ? 'Copied' : 'Copy'}
            </button>
            {!android && (
              <button onClick={() => void saveLog()} disabled={logEntries.length === 0}>
                Save…
              </button>
            )}
            <button onClick={() => void clearLog()} disabled={logEntries.length === 0}>
              Clear
            </button>
          </div>
        </div>
      )}

      {draft?.handoff_gate_choice && (
        <div className="field">
          <span>Moving chats to less-trusted providers</span>
          <p className="muted" style={{ margin: 0 }}>
            Kitty remembers your answer:{' '}
            {draft.handoff_gate_choice === 'keep'
              ? 'send the conversation along'
              : 'start clean on the new provider'}
            .
          </p>
          <div className="row">
            <button
              onClick={() =>
                void ipc
                  .patchConfig({ handoff_gate_choice: null })
                  .then(() => setHandoffReset(true))
                  .catch((e) => setRepairMsg(String(e)))
              }
            >
              Ask me every time
            </button>
          </div>
        </div>
      )}
      {handoffReset && <p className="muted">Kitty will ask every time again.</p>}

      <ClearChatHistory />
    </section>
  );
}
