import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import { describeOutput, outputKey } from '@/lib/outputs';
import { listingFor, useArtifactsStore } from '@/stores/artifactsStore';
import { useChatStore, type OutputRef } from '@/stores/chatStore';
import { DocumentIcon } from '@/components/icons/DocumentIcon';
import { FolderIcon } from '@/components/icons/FolderIcon';

/** What a turn left in the chat's `outputs` folder, one quiet card per
    top-level file or folder, between the answer and the message actions.

    Once the folder's listing is loaded it is the authority: a card whose entry
    has since been deleted is hidden, and the size and file-or-folder come from
    it rather than from what the turn recorded. */
export function OutputCards({ outputs }: { outputs: OutputRef[] }) {
  const cwd = useChatStore((s) => s.cwd);
  const entries = useArtifactsStore((s) => listingFor(s, cwd));
  const byName = entries ? new Map(entries.map((e) => [outputKey(e.name), e])) : null;

  const cards: OutputRef[] = [];
  for (const o of outputs) {
    if (!byName) {
      cards.push(o);
      continue;
    }
    const live = byName.get(outputKey(o.name));
    if (live) cards.push({ ...o, path: live.path, isDir: live.is_dir, size: live.size });
  }
  if (cards.length === 0) return null;

  return (
    <div className="output-cards">
      {cards.map((o) => (
        <OutputCard key={outputKey(o.name)} output={o} />
      ))}
    </div>
  );
}

function OutputCard({ output }: { output: OutputRef }) {
  const [error, setError] = useState<string | null>(null);
  const hasPane = useArtifactsStore((s) => s.hasPane);
  const setOpen = useArtifactsStore((s) => s.setOpen);
  const android = isAndroid();

  const act = async (p: Promise<unknown>, what: string) => {
    try {
      await p;
      setError(null);
    } catch (e) {
      setError(`Couldn't ${what}: ${String(e)}`);
    }
  };

  return (
    <div className="output-card">
      <div className="output-card-main">
        <span className="output-card-icon" aria-hidden="true">
          {output.isDir ? <FolderIcon /> : <DocumentIcon />}
        </span>
        <div className="output-card-text">
          {android ? (
            // Nothing on a phone can open a file inside the app's private
            // storage; Download is the way out, so the name is just a label.
            <span className="output-card-name" title={output.name}>
              {output.name}
            </span>
          ) : (
            <button
              type="button"
              className="output-card-name"
              title={`Open ${output.path}`}
              onClick={() => void act(ipc.openPath(output.path), 'open it')}
            >
              {output.name}
            </button>
          )}
          <span className="output-card-desc muted">{describeOutput(output)}</span>
        </div>
      </div>
      <div className="output-card-actions">
        {android ? (
          !output.isDir && (
            <button
              type="button"
              className="link"
              onClick={() => void act(ipc.downloadFile(output.path), 'download it')}
            >
              Download
            </button>
          )
        ) : hasPane ? (
          <button type="button" className="link" onClick={() => setOpen(true)}>
            Show in artifacts
          </button>
        ) : (
          <button
            type="button"
            className="link"
            onClick={() => void act(ipc.revealPath(output.path), 'show it')}
          >
            Show in folder
          </button>
        )}
      </div>
      {error && (
        <p className="output-card-error muted" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
