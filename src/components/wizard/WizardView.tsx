import { useEffect, useRef, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useRouteStore } from '@/stores/routeStore';
import type { Config } from '@/lib/types';
import { PathFork, type WizardPath } from './PathFork';
import { SupportModelsStep } from './SupportModelsStep';
import { isAndroid } from '@/lib/platform';
import { ApiKeyStep } from './ApiKeyStep';
import { ConfigureStep } from './ConfigureStep';
import { ModelDownloadStep } from './ModelDownloadStep';
import { DoneStep } from './DoneStep';
import { DEFAULT_URL } from '@/lib/provider_defaults';
import { defaultFor } from '@/lib/curated_models';

type StepId = 'path' | 'apikey' | 'configure' | 'model' | 'embedding' | 'support' | 'done';

/** Android's fixed sequence, with no path fork (docs/ANDROID.md §8.3).
 *
 * The desktop fork asks "run models on this computer, or use your own API
 * key?" — a question with only one real answer on a phone, since Android
 * never runs chat locally (D18). Offering it would invite the user down a
 * path that does not exist. What a phone actually needs is both things in
 * order: the support models Kitty runs itself, then a provider for chat.
 *
 * `configure` is dropped too: it sets a default context folder and a global
 * hotkey, neither of which Android has. */
export function androidSteps(): { id: StepId; label: string }[] {
  return [
    { id: 'support', label: 'Kitty’s models' },
    { id: 'apikey', label: 'Connect a provider' },
    { id: 'done', label: 'Done' },
  ];
}

export function stepsForPath(
  path: WizardPath | null,
  adaptivePathwayEnabled: boolean
): { id: StepId; label: string }[] {
  const start = { id: 'path' as const, label: 'Get started' };
  const done = { id: 'done' as const, label: 'Done' };
  // Shown on BOTH paths when adaptive-pathway is enabled — its embeddings run
  // on the in-process engine regardless of which provider serves chat, so an
  // API-key user needs this model too.
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
    { id: 'model', label: 'First model' },
    { id: 'configure', label: 'Configure' },
    ...embedding,
    done,
  ];
}

/** First-run wizard (also Settings → Setup & Repair).
 *
 * Desktop forks local-vs-API-key on the first screen and adapts the rest of
 * the flow to the choice. **Android has no fork** — see `androidSteps` — it
 * downloads Kitty's own support models, then connects a provider.
 *
 * Repair mode pre-selects the path from the current config and skips
 * straight past the fork. */
export function WizardView() {
  // Mode rides on the route (`routeStore` owns the `route://goto`
  // subscription), so opening Setup & Repair and being deep-linked into repair
  // mode are the same mechanism.
  const mode = useRouteStore((s) => s.wizardMode);
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

  // Keyed on `mode` so entering repair from an already-open wizard re-runs the
  // path inference rather than leaving the user on the setup fork.
  useEffect(() => {
    void ipc
      .getConfig()
      .then((c) => {
        setCfg(c);
        if (mode !== 'repair') return;
        // Repair mode: infer the path from what's already configured and
        // jump straight past the "welcome" fork.
        const active = c.providers.find((p) => p.id === c.active_provider_id);
        const inferred: WizardPath =
          active && active.provider_type !== 'local' ? 'api-key' : 'local';
        setPath(inferred);
        setStepIndex(1);
        setCompletedThrough(1);
      })
      .catch((e) => setLoadError(String(e)));
  }, [mode]);

  /** Create (once) and activate a provider profile bound to the in-process
      engine, so the model just downloaded is actually the one chat uses.
      The daemon registers this engine under a fixed id with no DB row of its
      own — Kitty's profile exists purely so the rest of the app's
      provider/model plumbing has something to point at. */
  const adoptLocalModel = async () => {
    const model = defaultFor('chat');
    const base = cfgRef.current;
    if (!model || !base) return;
    const existing = base.providers.find((p) => p.provider_type === 'local');
    try {
      const saved = await ipc.upsertProvider(
        {
          ...(existing ?? {
            id: '',
            name: 'On this device',
            provider_type: 'local',
            base_url: DEFAULT_URL.local,
            is_trusted: true,
            strip_reasoning: false,
            supports_vision: false,
            created_at: '',
          }),
          models: [model.file.replace(/\.gguf$/i, '')],
        } as Parameters<typeof ipc.upsertProvider>[0],
        null
      );
      await ipc.activateProvider(saved.id);
    } catch (e) {
      setSaveError(String(e));
    }
  };

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

  // Android gets a fixed two-step flow; desktop keeps the fork.
  const steps = isAndroid() ? androidSteps() : stepsForPath(path, cfg.adaptive_pathway_enabled);
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

      {current === 'support' && <SupportModelsStep onNext={nextAndMark} onSkip={nextAndMark} />}
      {current === 'path' && (
        <PathFork
          mode={mode}
          selected={path}
          onSelect={(p) => {
            if (p !== path) setCompletedThrough(0);
            setPath(p);
            nextAndMark();
          }}
        />
      )}
      {current === 'apikey' && <ApiKeyStep onBack={back} onNext={nextAndMark} />}
      {current === 'configure' && (
        <ConfigureStep cfg={cfg} saveCfg={saveCfg} onBack={back} onNext={nextAndMark} />
      )}
      {current === 'model' && (
        <ModelDownloadStep
          role="chat"
          title="Download a model"
          blurb="Kitty runs this model itself — nothing else to install, and it works offline."
          skipNote="You can skip and add an API key later, but chat won't work until one or the other is set up."
          onBack={back}
          onNext={() => void adoptLocalModel().then(nextAndMark)}
          onSkip={nextAndMark}
        />
      )}
      {current === 'embedding' && (
        <ModelDownloadStep
          role="embedding"
          title="Memory model"
          blurb="Lets Kitty remember how you work across sessions with real semantic recall."
          skipNote="Optional — without it, memory falls back to keyword matching."
          onBack={back}
          onNext={nextAndMark}
          onSkip={nextAndMark}
        />
      )}
      {current === 'done' && <DoneStep path={path} onBack={back} />}
    </div>
  );
}
