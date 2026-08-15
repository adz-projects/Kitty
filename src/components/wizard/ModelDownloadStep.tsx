import { useState } from 'react';
import { ModelDownloadCard } from './ModelDownloadCard';
import type { CuratedModel } from '@/lib/curated_models';

interface Props {
  role: CuratedModel['role'];
  title: string;
  blurb: string;
  /** Shown under the button when the user can safely move on without this. */
  skipNote?: string;
  onBack: () => void;
  onNext: () => void;
  onSkip: () => void;
}

/** Download one curated GGUF, with step navigation around it.
 *
 * Replaces the wizard's old detect-and-install-Ollama pair: there is no
 * third-party installer to run any more, no UAC prompt, and no version to
 * detect. Just a file.
 *
 * The card itself lives in `ModelDownloadCard` because Android's first run
 * shows two at once (`SupportModelsStep`). */
export function ModelDownloadStep({ role, title, blurb, skipNote, onBack, onNext, onSkip }: Props) {
  const [installed, setInstalled] = useState(false);

  return (
    <div className="wizard-body">
      <h1>{title}</h1>
      <p className="muted">{blurb}</p>

      <ModelDownloadCard role={role} onInstalledChange={setInstalled} />

      {skipNote && !installed && <p className="muted">{skipNote}</p>}

      <div className="row">
        <button onClick={onBack}>Back</button>
        {installed ? (
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
