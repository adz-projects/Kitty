import { useEffect } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore, type Artifact } from '@/stores/chatStore';
import { DocumentIcon } from '@/components/icons/DocumentIcon';

const POLL_INTERVAL_MS = 5000;

/** Collapsible right-side pane listing files in the chat folder this session,
    derived from tool-call events plus a disk scan (CLAUDE.md Phase 4, Round-7
    item 5) — the tool-call path alone misses files that land in the folder
    without going through a tracked tool call (e.g. dropped in via Explorer).
    Persists nothing app-side.

    Polls the folder continuously (not just on mount/cwd-change) so it stays
    live while the pane sits open: additions and deletions made outside a
    tracked tool call (Explorer, another app, a shell the agent isn't
    supervising) both show up within one poll tick, not just at the next
    session switch. Runs unconditionally on `cwd`, not gated on the current
    artifact count — an empty folder that gains its first file needs the
    add-scan to run too, not just the prune side. */
export function ArtifactsPane() {
  const artifacts = useChatStore((s) => s.artifacts);
  const cwd = useChatStore((s) => s.cwd);
  const clearArtifacts = useChatStore((s) => s.clearArtifacts);
  const pruneMissingArtifacts = useChatStore((s) => s.pruneMissingArtifacts);
  const refreshArtifactsFromDisk = useChatStore((s) => s.refreshArtifactsFromDisk);

  useEffect(() => {
    if (!cwd) return;
    void refreshArtifactsFromDisk();
    const id = setInterval(() => {
      void refreshArtifactsFromDisk();
      void pruneMissingArtifacts();
    }, POLL_INTERVAL_MS);
    return () => clearInterval(id);
  }, [cwd, refreshArtifactsFromDisk, pruneMissingArtifacts]);

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
