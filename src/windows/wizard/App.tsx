import { useEffect, useState } from 'react';
import { ipc, onPullProgress, onWizardNavigate, pickFolder } from '@/lib/ipc';
import type { Config, Detection, PullProgress } from '@/lib/types';
import { STARTER_MODELS } from '@/lib/starter_models';

const STEPS = ['Detect', 'Configure', 'First model', 'Done'] as const;

/** First-run wizard (also Settings → Setup & Repair). Walks a new user from
    "nothing installed" to "ready to chat". Repair mode pre-runs detection. */
export function App() {
  const [step, setStep] = useState(0);
  const [mode, setMode] = useState<'setup' | 'repair'>('setup');
  const [det, setDet] = useState<Detection | null>(null);
  const [detecting, setDetecting] = useState(false);
  const [cfg, setCfg] = useState<Config | null>(null);

  const detect = async () => {
    setDetecting(true);
    try {
      setDet(await ipc.detectDependencies());
    } finally {
      setDetecting(false);
    }
  };

  useEffect(() => {
    void detect();
    void ipc.getConfig().then(setCfg);
    void ipc.getWizardMode().then((m) => {
      if (m === 'repair') setMode('repair');
    });
    const un = onWizardNavigate((m) => setMode(m === 'repair' ? 'repair' : 'setup'));
    return () => void un.then((fn) => fn());
  }, []);

  const saveCfg = async (patch: Partial<Config>) => {
    if (!cfg) return;
    const next = { ...cfg, ...patch };
    setCfg(next);
    await ipc.setConfig(next);
  };

  return (
    <div className="window-root wizard">
      <div className="wizard-steps">
        {STEPS.map((s, i) => (
          <span
            key={s}
            className={`wizard-step${i === step ? ' active' : ''}${i < step ? ' done' : ''}`}
          >
            {i + 1}. {s}
          </span>
        ))}
      </div>

      {step === 0 && (
        <section className="wizard-panel">
          <h1>{mode === 'repair' ? 'Repair setup' : 'Welcome to Kitty'}</h1>
          <p className="muted">Let’s make sure Ollama and Goose are installed.</p>
          {detecting && <p>Detecting…</p>}
          {det && (
            <>
              <DepRow name="Ollama" dep={det.ollama} which="ollama" onChanged={detect} />
              <DepRow name="Goose" dep={det.goose} which="goose" onChanged={detect} />
            </>
          )}
          <div className="wizard-actions">
            <button onClick={() => void detect()}>Re-detect</button>
            <button
              className="primary"
              disabled={!det || !det.ollama.installed || !det.goose.installed}
              onClick={() => setStep(1)}
            >
              Next
            </button>
          </div>
        </section>
      )}

      {step === 1 && cfg && (
        <section className="wizard-panel">
          <h1>Configure</h1>
          <label className="field">
            <span>Ollama endpoint</span>
            <input
              value={cfg.ollama_base_url}
              onChange={(e) => setCfg({ ...cfg, ollama_base_url: e.target.value })}
            />
          </label>
          <label className="field">
            <span>Default context folder</span>
            <div className="row">
              <input
                value={cfg.default_context_folder ?? ''}
                placeholder="Documents\\Goose"
                onChange={(e) => setCfg({ ...cfg, default_context_folder: e.target.value || null })}
              />
              <button
                onClick={async () => {
                  const d = await pickFolder();
                  if (d) setCfg({ ...cfg, default_context_folder: d });
                }}
              >
                Browse…
              </button>
            </div>
          </label>
          <label className="field">
            <span>Hotkey</span>
            <input
              value={cfg.hotkeys[0] ?? ''}
              onChange={(e) =>
                setCfg({ ...cfg, hotkeys: [e.target.value, ...cfg.hotkeys.slice(1)] })
              }
            />
            <small className="muted">
              The Copilot key is used automatically if your keyboard has one.
            </small>
          </label>
          <div className="wizard-actions">
            <button onClick={() => setStep(0)}>Back</button>
            <button
              className="primary"
              onClick={async () => {
                await saveCfg({});
                setStep(2);
              }}
            >
              Next
            </button>
          </div>
        </section>
      )}

      {step === 2 && <FirstModel onBack={() => setStep(1)} onNext={() => setStep(3)} />}

      {step === 3 && (
        <section className="wizard-panel">
          <h1>You’re all set 🎉</h1>
          <p className="muted">
            Press your hotkey (or the Copilot key) any time to summon Kitty. You can re-run this
            from Settings → Setup &amp; Repair.
          </p>
          <div className="wizard-actions">
            <button className="primary" onClick={() => void ipc.completeSetup()}>
              Start chatting
            </button>
          </div>
        </section>
      )}
    </div>
  );
}

const RELEASES_URL: Record<'ollama' | 'goose', string> = {
  ollama: 'https://github.com/ollama/ollama/releases/latest',
  goose: 'https://github.com/aaif-goose/goose/releases/latest',
};

function DepRow({
  name,
  dep,
  which,
  onChanged,
}: {
  name: string;
  dep: {
    installed: boolean;
    version: string | null;
    latest_version: string | null;
    is_outdated: boolean | null;
  };
  which: 'ollama' | 'goose';
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  return (
    <div className="dep-row">
      <div>
        <strong>{name}</strong>{' '}
        {dep.installed ? (
          <span className="status-badge">✓ {dep.version ?? 'installed'}</span>
        ) : (
          <span className="status-badge">not found</span>
        )}
        {dep.is_outdated && (
          <>
            {' '}
            <span className="status-badge status-badge-warn">
              Update available: {dep.latest_version}
            </span>
          </>
        )}
      </div>
      {/* Goose has no Windows .exe/.msi installer — only plain zip archives
          (confirmed via its GitHub releases). A button that calls
          installDependency() would only ever throw; point straight at the
          release page with instructions instead of faking automation that
          doesn't exist. Ollama does ship a real silent-ish installer, so it
          keeps the one-click flow. */}
      {!dep.installed && which === 'ollama' && (
        <button
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              await ipc.installDependency(which);
            } catch (e) {
              alert(String(e));
            } finally {
              setBusy(false);
              onChanged();
            }
          }}
        >
          {busy ? 'Launching installer…' : 'Install'}
        </button>
      )}
      {!dep.installed && which === 'goose' && (
        <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: 4 }}>
          <button onClick={() => void ipc.openPath(RELEASES_URL.goose)}>Get Goose ↗</button>
          <small className="muted">
            Download <code>goose-x86_64-pc-windows-msvc.zip</code> and extract it anywhere, then
            re-check. (Skip the <code>Goose-win32-x64.zip</code> asset — that's the separate Goose
            Desktop app, which Kitty warns about if it's also running.)
          </small>
        </div>
      )}
      {dep.is_outdated && (
        <button onClick={() => void ipc.openPath(RELEASES_URL[which])}>View release</button>
      )}
    </div>
  );
}

function FirstModel({ onBack, onNext }: { onBack: () => void; onNext: () => void }) {
  const [selected, setSelected] = useState(STARTER_MODELS[0].tag);
  const [progress, setProgress] = useState<PullProgress | null>(null);
  const [installed, setInstalled] = useState<string[]>([]);

  useEffect(() => {
    void ipc.ollamaListModels().then((m) => setInstalled(m.map((x) => x.name)));
    const un = onPullProgress((p) => {
      setProgress(p);
      if (p.done && !p.error)
        void ipc.ollamaListModels().then((m) => setInstalled(m.map((x) => x.name)));
    });
    return () => void un.then((fn) => fn());
  }, []);

  const have = installed.includes(selected);
  const pct =
    progress?.total && progress?.completed
      ? Math.round((progress.completed / progress.total) * 100)
      : null;
  // Installed models that aren't in the curated starter list (Round-2 item 17) —
  // offered as ready-to-use options so the user needn't download a starter.
  const starterTags = new Set(STARTER_MODELS.map((m) => m.tag));
  const otherInstalled = installed.filter((name) => !starterTags.has(name));

  return (
    <section className="wizard-panel">
      <h1>Pick a starter model</h1>
      <p className="muted">All under 4B parameters — they run on modest hardware.</p>
      <div className="starter-list">
        {STARTER_MODELS.map((m) => (
          <label key={m.tag} className={`starter${selected === m.tag ? ' selected' : ''}`}>
            <input
              type="radio"
              name="model"
              checked={selected === m.tag}
              onChange={() => setSelected(m.tag)}
            />
            <div>
              <div>
                <strong>{m.label}</strong> <span className="muted">~{m.size_gb} GB</span>
                {installed.includes(m.tag) && <span className="status-badge">installed</span>}
              </div>
              <div className="muted" style={{ fontSize: 12 }}>
                {m.blurb}
              </div>
            </div>
          </label>
        ))}
      </div>

      {otherInstalled.length > 0 && (
        <>
          <p className="muted">Already installed on this machine:</p>
          <div className="starter-list">
            {otherInstalled.map((name) => (
              <label key={name} className={`starter${selected === name ? ' selected' : ''}`}>
                <input
                  type="radio"
                  name="model"
                  checked={selected === name}
                  onChange={() => setSelected(name)}
                />
                <div>
                  <strong>{name}</strong> <span className="status-badge">installed</span>
                </div>
              </label>
            ))}
          </div>
        </>
      )}

      {progress && (
        <div className="pull-row">
          <div className="pull-head">
            <span>{progress.model}</span>
            <span className="muted">
              {progress.error ? `error: ${progress.error}` : progress.status}
            </span>
          </div>
          <div className="progress">
            <div
              className="progress-bar"
              style={{ width: progress.done ? '100%' : pct != null ? `${pct}%` : '30%' }}
            />
          </div>
        </div>
      )}

      <div className="wizard-actions">
        <button onClick={onBack}>Back</button>
        {have ? (
          <button className="primary" onClick={onNext}>
            Use this model →
          </button>
        ) : (
          <button className="primary" onClick={() => void ipc.ollamaPullModel(selected)}>
            Download
          </button>
        )}
      </div>
    </section>
  );
}
