import { ipc } from '@/lib/ipc';
import { useChatStore, type Artifact } from '@/stores/chatStore';

/** Collapsible right-side pane listing files the agent produced this session,
    derived from tool-call events (CLAUDE.md Phase 4). Persists nothing app-side. */
export function ArtifactsPane() {
  const artifacts = useChatStore((s) => s.artifacts);
  return (
    <aside className="artifacts-pane">
      <div className="artifacts-head">Artifacts ({artifacts.length})</div>
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
        📄 {artifact.name}
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
