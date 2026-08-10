export type WizardPath = 'local' | 'api-key';

/** The wizard's first screen: local (Ollama-backed) vs. bring-your-own API
    key. Written for someone who's used Claude.ai/Copilot but has never heard
    of Ollama — no jargon, one plain sentence per option. */
export function PathFork({
  mode,
  selected,
  onSelect,
}: {
  mode: 'setup' | 'repair';
  selected: WizardPath | null;
  onSelect: (path: WizardPath) => void;
}) {
  return (
    <section className="wizard-panel">
      <h1>{mode === 'repair' ? 'Repair setup' : 'Welcome to Kitty'}</h1>
      <p className="muted">
        {mode === 'repair'
          ? "Let's get your setup working again."
          : 'How should Kitty answer your questions?'}
      </p>
      <div className="path-fork">
        <button
          type="button"
          className={`path-card${selected === 'local' ? ' selected' : ''}`}
          onClick={() => onSelect('local')}
        >
          <strong>Run models on this computer</strong>
          <span className="muted">
            Free and completely private — nothing you type ever leaves your machine. Needs a
            one-time download (a couple of GB) and a reasonably modern PC.
          </span>
        </button>
        <button
          type="button"
          className={`path-card${selected === 'api-key' ? ' selected' : ''}`}
          onClick={() => onSelect('api-key')}
        >
          <strong>Use my own API key</strong>
          <span className="muted">
            Connect an account you already have with Anthropic, OpenAI, OpenRouter, or another
            provider — the same way you'd use Claude.ai or Copilot, just from Kitty.
          </span>
        </button>
      </div>
    </section>
  );
}
