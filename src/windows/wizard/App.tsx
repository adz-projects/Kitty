import { useEffect, useRef, useState } from 'react';
import { ipc, onWizardNavigate } from '@/lib/ipc';
import type { Config } from '@/lib/types';
import { PathFork, type WizardPath } from './PathFork';
import { DetectStep } from './DetectStep';
import { ApiKeyStep } from './ApiKeyStep';
import { ConfigureStep } from './ConfigureStep';
import { FirstModelStep } from './FirstModelStep';
import { EmbeddingModelStep } from './EmbeddingModelStep';
import { DoneStep } from './DoneStep';

type StepId = 'path' | 'detect' | 'apikey' | 'configure' | 'model' | 'embedding' | 'done';

export function stepsForPath(
  path: WizardPath | null,
  adaptivePathwayEnabled: boolean
): { id: StepId; label: string }[] {
  const start = { id: 'path' as const, label: 'Get started' };
  const done = { id: 'done' as const, label: 'Done' };
  // Shown on BOTH paths when adaptive-pathway is enabled — its embeddings are
  // local-Ollama-only regardless of chat provider (see EmbeddingModelStep).
  const embedding = adaptivePathwayEnabled
    ? [{ id: 'embedding' as const, label: 'Learning model' }]
    : [];
  if (path === 'api-key') {
    return [
      start,
      { id: 'apikey', label: 'Connect' },
      { id: 'configure', label: 'Configure' },
      ...embedding,
      done,
    ];
  }
  return [
    start,
    { id: 'detect', label: 'Detect' },
    { id: 'configure', label: 'Configure' },
    { id: 'model', label: 'First model' },
    ...embedding,
    done,
  ];
}

/** First-run wizard (also Settings → Setup & Repair). First screen forks
    local-vs-API-key; the rest of the flow adapts to whichever the user
    picked. Repair mode pre-selects the path from the current config and
    skips straight past the fork. */
export function App() {
  const [mode, setMode] = useState<'setup' | 'repair'>('setup');
  const [cfg, setCfg] = useState<Config | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [path, setPath] = useState<WizardPath | null>(null);
  const [stepIndex, setStepIndex] = useState(0);
  const [completedThrough, setCompletedThrough] = useState(0);

  // Mirrors `cfg` synchronously so back-to-back saveCfg calls (e.g. rapid
  // keystrokes) each merge onto the latest patch instead of the stale `cfg`
  // closure from whichever render they were created in.
  const cfgRef = useRef<Config | null>(null);
  cfgRef.current = cfg;
  // Serializes the actual disk writes so concurrent ipc.setConfig calls can't
  // resolve out of order and leave a stale value persisted.
  const writeQueueRef = useRef<Promise<void>>(Promise.resolve());

  useEffect(() => {
    void ipc
      .getWizardMode()
      .then((m) => {
        const repair = m === 'repair';
        if (repair) setMode('repair');
        return ipc.getConfig().then((c) => {
          setCfg(c);
          if (!repair) return;
          // Repair mode: infer the path from what's already configured and
          // jump straight past the "welcome" fork.
          const active = c.providers.find((p) => p.id === c.active_provider_id);
          const inferred: WizardPath =
            active && active.provider_type !== 'ollama' ? 'api-key' : 'local';
          setPath(inferred);
          setStepIndex(1);
          setCompletedThrough(1);
        });
      })
      .catch((e) => setLoadError(String(e)));
    const un = onWizardNavigate((m) => setMode(m === 'repair' ? 'repair' : 'setup'));
    return () => void un.then((fn) => fn());
  }, []);

  const saveCfg = (patch: Partial<Config>) => {
    const base = cfgRef.current;
    if (!base) return Promise.resolve();
    const next = { ...base, ...patch };
    cfgRef.current = next;
    setCfg(next);
    const run = writeQueueRef.current.then(() => ipc.setConfig(next));
    writeQueueRef.current = run.catch(() => {});
    return run.catch((e) => {
      setSaveError(String(e));
      throw e;
    });
  };

  if (loadError) {
    return (
      <div className="window-root wizard">
        <p className="error">Couldn't load setup: {loadError}</p>
      </div>
    );
  }

  if (!cfg) {
    return (
      <div className="window-root wizard">
        <p className="muted">Loading…</p>
      </div>
    );
  }

  const steps = stepsForPath(path, cfg.adaptive_pathway_enabled);
  const current = steps[stepIndex]?.id ?? 'path';

  const next = () => setStepIndex((i) => Math.min(i + 1, steps.length - 1));
  const nextAndMark = () => {
    setCompletedThrough((c) => Math.max(c, stepIndex + 1));
    next();
  };
  const back = () => setStepIndex((i) => Math.max(i - 1, 0));

  return (
    <div className="window-root wizard">
      {saveError && (
        <p className="error" onClick={() => setSaveError(null)}>
          Couldn't save: {saveError}
        </p>
      )}
      <div className="wizard-steps">
        {steps.map((s, i) => (
          <button
            key={s.id}
            type="button"
            className={`wizard-step${i === stepIndex ? ' active' : ''}${i < stepIndex ? ' done' : ''}`}
            disabled={i > completedThrough}
            onClick={() => i <= completedThrough && setStepIndex(i)}
          >
            {i + 1}. {s.label}
          </button>
        ))}
      </div>

      {current === 'path' && (
        <PathFork
          mode={mode}
          selected={path}
          onSelect={async (p) => {
            if (p !== path) setCompletedThrough(0);
            setPath(p);
            // Ollama is still required on the api-key path when
            // adaptive-pathway is enabled (its embeddings are
            // local-Ollama-only regardless of chat provider) — only force it
            // off when AP won't need it either, so the toggle in Settings →
            // Advanced doesn't read as "off" while Ollama is actually running.
            try {
              await saveCfg({ ollama_enabled: p === 'local' || cfg.adaptive_pathway_enabled });
            } catch {
              return; // saveError banner already shown; don't advance on a failed save
            }
            nextAndMark();
          }}
        />
      )}
      {current === 'detect' && <DetectStep onBack={back} onNext={nextAndMark} />}
      {current === 'apikey' && <ApiKeyStep onBack={back} onNext={nextAndMark} />}
      {current === 'configure' && (
        <ConfigureStep
          cfg={cfg}
          saveCfg={saveCfg}
          showOllamaEndpoint={path === 'local'}
          onBack={back}
          onNext={nextAndMark}
        />
      )}
      {current === 'model' && (
        <FirstModelStep onBack={back} onNext={nextAndMark} onSkip={nextAndMark} />
      )}
      {current === 'embedding' && (
        <EmbeddingModelStep cfg={cfg} onBack={back} onNext={nextAndMark} onSkip={nextAndMark} />
      )}
      {current === 'done' && <DoneStep path={path} onBack={back} />}
    </div>
  );
}
