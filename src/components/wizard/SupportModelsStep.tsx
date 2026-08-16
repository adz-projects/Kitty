import { useState } from 'react';
import { ModelDownloadCard } from './ModelDownloadCard';

/** Android's first wizard step: the one model Kitty runs *itself*
    (docs/ANDROID.md §8.3).
 *
 * Android never runs chat locally (D18), so the desktop fork between
 * "run models on this computer" and "use my own API key" is a false choice
 * here — a phone always needs a provider for chat. What it *does* run locally
 * is the memory embedder. There is deliberately **no local summarizer on
 * Android**: after the LiteRT migration, compaction offloads to the session's
 * remote chat model (no generative model runs on the phone), so the summarizer
 * `.litertlm` is a Windows-only download and must not appear here.
 *
 * The embedder is genuinely optional and degrades rather than fails — memory
 * falls back to hash-space embeddings (D4) — which is why Skip is a
 * first-class action, not a disabled button. */
export function SupportModelsStep({ onNext, onSkip }: { onNext: () => void; onSkip: () => void }) {
  const [embedding, setEmbedding] = useState(false);

  return (
    <div className="wizard-body">
      <h1>Download Kitty&apos;s memory model</h1>
      <p className="muted">
        This runs on your phone and does background work — it doesn&apos;t answer your questions.
        You&apos;ll connect a provider for that next.
      </p>

      <h2>Memory</h2>
      <p className="muted">Lets Kitty remember how you work across conversations.</p>
      <ModelDownloadCard role="embedding" onInstalledChange={setEmbedding} />

      {!embedding && (
        <p className="muted">
          Optional — you can skip and add it later from Settings. Without the memory model, recall
          falls back to keyword matching.
        </p>
      )}

      <div className="row">
        {embedding ? (
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
