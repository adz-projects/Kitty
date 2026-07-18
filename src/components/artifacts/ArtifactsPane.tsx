import { useEffect } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore, type Artifact } from '@/stores/chatStore';
import { DocumentIcon } from '@/components/icons/DocumentIcon';

const PRUNE_INTERVAL_MS = 5000;

/** Collapsible right-side pane listing files the agent produced this session,
    derived from tool-call events (CLAUDE.md Phase 4). Persists nothing app-side. */
export function ArtifactsPane() {
  const artifacts = useChatStore((s) => s.artifacts);
  const clearArtifacts = useChatStore((s) => s.clearArtifacts);
  const pruneMissingArtifacts = useChatStore((s) => s.pruneMissingArtifacts);

  // A file can disappear from the chat directory either because a tool call
  // deleted it or because the user deleted it out-of-band (Explorer, another
  // app) — periodic existence checks catch both without needing to special-
  // case delete-shaped tool calls.
  useEffect(() => {
    if (artifacts.length === 0) return;
    const id = setInterval(() => void pruneMissingArtifacts(), PRUNE_INTERVAL_MS);
    return () => clearInterval(id);
  }, [artifacts.length, pruneMissingArtifacts]);

  return (
    <aside className="artifacts-pane">
      <div className="artifacts-head">
        <span>Artifacts ({artifacts.length})</span>
        {artifacts.length > 0 && (
          <button
            className="link"
            title="Clear the list (does not delete the files)"
            onClick={clearArtifacts}
          >
            Clear
          </button>
        )}
      </div>
      {artifacts.length === 0 ? (
        <p className="muted" style={{ fontSize: 12, padding: '4px 8px' }}>
          Files the agent creates or edits will appear here.
        </p>
      ) : (
        <div className="artifacts-list">
          {artifacts.map((a) => (
            <ArtifactCard key={a.path} artifact={a} />
          ))}
        </div>
      )}
    </aside>
  );
}

function ArtifactCard({ artifact }: { artifact: Artifact }) {
  return (
    <div className="artifact-card">
      <div className="artifact-name" title={artifact.path}>
        <DocumentIcon /> {artifact.name}
      </div>
      <div className="artifact-path muted" title={artifact.path}>
        {artifact.path}
      </div>
      <div className="artifact-actions">
        <button onClick={() => void ipc.openPath(artifact.path)}>Open</button>
        <button onClick={() => void ipc.revealPath(artifact.path)}>Show in folder</button>
        <button onClick={() => void navigator.clipboard.writeText(artifact.path)}>Copy path</button>
      </div>
    </div>
  );
}
