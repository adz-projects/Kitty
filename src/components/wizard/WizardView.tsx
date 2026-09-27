import { useEffect, useRef, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useRouteStore } from '@/stores/routeStore';
import { useBackDismiss } from '@/hooks/useBackDismiss';
import type { Config, SetupValidation } from '@/lib/types';
import { SupportModelsStep } from './SupportModelsStep';
import { isAndroid } from '@/lib/platform';
import { ApiKeyStep } from './ApiKeyStep';
import { ConfigureStep } from './ConfigureStep';
import { ModelDownloadStep } from './ModelDownloadStep';
import { DoneStep } from './DoneStep';

type StepId = 'apikey' | 'configure' | 'embedding' | 'summarizer' | 'support' | 'done';

/** Android's fixed sequence, with no path fork (docs/ANDROID.md §8.3).
 *
 * Android never runs chat locally (D18): what a phone actually needs is both
 * things in order — the memory model Kitty runs itself, then a provider for
 * chat.
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

/** Desktop's fixed sequence.
 *
 * There used to be a first-screen fork ("run models on this computer, or use
 * your own API key?"), because Kitty could run chat entirely in-process via
 * llama.cpp. That local-chat engine is gone — replaced by LiteRT, which does
 * only embeddings (and, on Windows, compaction summarization), never chat —
 * so "on this computer" was never a real answer for chat and offering it sent
 * a newcomer down a dead path (a "local" provider Kitty can no longer serve).
 * Every install needs a provider, so the wizard just asks for one. */
export function desktopSteps(adaptivePathwayEnabled: boolean): { id: StepId; label: string }[] {
  // Its embeddings run on the in-process LiteRT engine regardless of which
  // provider serves chat, so this is offered unconditionally when the pathway
  // engine is on — not gated on how the user connects for chat.
  const embedding = adaptivePathwayEnabled
    ? [{ id: 'embedding' as const, label: 'Learning model' }]
    : [];
  return [
    { id: 'apikey', label: 'Connect' },
    { id: 'configure', label: 'Configure' },
    ...embedding,
    // Optional but recommended (decision #62): summarizing long chats on this
    // computer instead of paying the chat provider to do it.
    { id: 'summarizer', label: 'Summarizer' },
    { id: 'done', label: 'Done' },
  ];
}

/** Where repair opens (#63): the step that fixes what `validate_setup` found
    broken. The engine has no step of its own — the last step re-checks and
    says what is wrong. Pure. */
export function repairStepIndex(
  steps: { id: StepId }[],
  broken: SetupValidation['first_broken_step']
): number {
  const find = (id: StepId) => steps.findIndex((s) => s.id === id);
  const at =
    broken === 'provider'
      ? find('apikey')
      : broken === 'models'
        ? Math.max(find('embedding'), find('support'))
        : broken === 'engine'
          ? find('done')
          : -1;
  return at >= 0 ? at : 0;
}

/** First-run wizard (also Settings → Setup & Repair).
 *
 * Desktop and Android both run a single fixed sequence now — see
 * `desktopSteps`/`androidSteps` for why there's no local-vs-API-key fork on
 * either platform. */
export function WizardView() {
  // Mode rides on the route (`routeStore` owns the `route://goto`
  // subscription), so opening Setup & Repair and being deep-linked into repair
  // mode are the same mechanism.
  const mode = useRouteStore((s) => s.wizardMode);
  const [cfg, setCfg] = useState<Config | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [stepIndex, setStepIndex] = useState(0);
  const [completedThrough, setCompletedThrough] = useState(0);

  // Mirrors `cfg` synchronously so back-to-back saveCfg calls (e.g. rapid
  // keystrokes) each merge onto the latest patch instead of the stale `cfg`
  // closure from whichever render they were created in.
  const cfgRef = useRef<Config | null>(null);
  cfgRef.current = cfg;
  // Serializes the actual disk writes so concurrent patches can't resolve out
  // of order and leave a stale value persisted.
  const writeQueueRef = useRef<Promise<void>>(Promise.resolve());

  const repair = mode === 'repair';
  const goto = useRouteStore((s) => s.goto);
  // Repair is a detour from Settings, so there's always a way back (#63).
  const cancel = () => goto('settings', { section: 'advanced' });
  useBackDismiss(isAndroid() && repair, cancel);

  useEffect(() => {
    void ipc
      .getConfig()
      .then((c) => {
        setCfg(c);
        if (mode !== 'repair') return;
        // Every step is open in repair, and it starts at the broken one.
        const steps = isAndroid() ? androidSteps() : desktopSteps(c.adaptive_pathway_enabled);
        setCompletedThrough(steps.length);
        void ipc
          .validateSetup()
          .then((v) => setStepIndex(repairStepIndex(steps, v.first_broken_step)))
          .catch(() => {});
      })
      .catch((e) => setLoadError(String(e)));
  }, [mode]);

  const saveCfg = (patch: Partial<Config>) => {
    const base = cfgRef.current;
    if (!base) return Promise.resolve();
    const next = { ...base, ...patch };
    cfgRef.current = next;
    setCfg(next);
    const run = writeQueueRef.current.then(() => ipc.patchConfig(patch));
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

  const steps = isAndroid() ? androidSteps() : desktopSteps(cfg.adaptive_pathway_enabled);
  const current = steps[stepIndex]?.id ?? steps[0]?.id;

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

      {repair && (
        <div className="row" style={{ justifyContent: 'flex-end' }}>
          <button onClick={cancel}>Cancel repair</button>
        </div>
      )}

      {current === 'support' && <SupportModelsStep onNext={nextAndMark} onSkip={nextAndMark} />}
      {current === 'apikey' && <ApiKeyStep onBack={back} onNext={nextAndMark} repair={repair} />}
      {current === 'configure' && (
        <ConfigureStep cfg={cfg} saveCfg={saveCfg} onBack={back} onNext={nextAndMark} />
      )}
      {current === 'embedding' && (
        <ModelDownloadStep
          role="embedding"
          title="Memory model"
          blurb="Lets Kitty remember how you work across sessions with real semantic recall."
          skipNote="Optional — without it, chat works as normal and memory stays off."
          onBack={back}
          onNext={nextAndMark}
          onSkip={nextAndMark}
        />
      )}
      {current === 'summarizer' && (
        <ModelDownloadStep
          role="chat"
          title="Summarizer model (recommended)"
          blurb="Long chats get summarized so they don't run out of room. With this model it happens here on your computer — private, and free. Without it, your chat provider does it, which costs tokens."
          skipNote="Optional — you can download it later in Settings → Helper Models."
          onBack={back}
          onNext={nextAndMark}
          onSkip={nextAndMark}
        />
      )}
      {current === 'done' && <DoneStep onBack={back} />}
    </div>
  );
}
