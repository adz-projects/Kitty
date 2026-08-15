import { useState } from 'react';
import { ModelDownloadCard } from './ModelDownloadCard';

/** Android's first wizard step: the two models Kitty runs *itself*, together
    (docs/ANDROID.md §8.3).
 *
 * Android never runs chat locally (D18), so the desktop fork between
 * "run models on this computer" and "use my own API key" is a false choice
 * here — a phone always needs a provider for chat. What it *does* run locally
 * is the pair of support models: the summarizer that compacts long
 * conversations, and the embedder behind memory. Presenting those as a
 * download step, then the provider as a second step, matches what actually
 * happens instead of asking the user to pick a path only one side of which
 * exists.
 *
 * Both are genuinely optional and degrade rather than fail — the summarizer
 * falls back to the session model (D12) and memory falls back to hash-space
 * embeddings (D4) — which is why Skip is a first-class action, not a
 * disabled button. */
export function SupportModelsStep({ onNext, onSkip }: { onNext: () => void; onSkip: () => void }) {
  const [summarizer, setSummarizer] = useState(false);
  const [embedding, setEmbedding] = useState(false);
  const both = summarizer && embedding;

  return (
    <div className="wizard-body">
      <h1>Download Kitty&apos;s own models</h1>
      <p className="muted">
        These two run on your phone and do the background work — they don&apos;t answer your
        questions. You&apos;ll connect a provider for that next.
      </p>

      <h2>Summarizer</h2>
      <p className="muted">
        Keeps long conversations going by condensing older messages instead of dropping them.
      </p>
      <ModelDownloadCard role="chat" onInstalledChange={setSummarizer} />

      <h2>Memory</h2>
      <p className="muted">Lets Kitty remember how you work across conversations.</p>
      <ModelDownloadCard role="embedding" onInstalledChange={setEmbedding} />

      {!both && (
        <p className="muted">
          Optional — you can skip and add them later from Settings. Without the summarizer, long
          chats get trimmed; without the memory model, recall falls back to keyword matching.
        </p>
      )}

      <div className="row">
        {both ? (
          <button className="primary" onClick={onNext}>
            Continue
          </button>
        ) : (
          <button onClick={onSkip}>Skip for now</button>
        )}
      </div>
    </div>
  );
}
