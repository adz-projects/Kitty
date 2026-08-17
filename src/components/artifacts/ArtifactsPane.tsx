import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { isAndroid } from '@/lib/platform';
import { useChatStore, type Artifact } from '@/stores/chatStore';
import { useRouteStore } from '@/stores/routeStore';
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
/** `onClose` is only supplied on mobile, where this renders as a sheet over
    the conversation (see `base.css`'s mobile block) and therefore covers the
    header button that opened it. Desktop is a third column with nothing
    obscured, so it passes nothing and no close button appears. */
export function ArtifactsPane({ onClose }: { onClose?: () => void } = {}) {
  const artifacts = useChatStore((s) => s.artifacts);
  const cwd = useChatStore((s) => s.cwd);
  const pruneMissingArtifacts = useChatStore((s) => s.pruneMissingArtifacts);
  const refreshArtifactsFromDisk = useChatStore((s) => s.refreshArtifactsFromDisk);
  // The hub keeps the whole chat workspace mounted (hidden) while the user is
  // on the Settings/Wizard route — the poll must not keep scanning disk for a
  // pane nobody can see. (`document.hidden` alone only covers minimization.)
  const view = useRouteStore((s) => s.view);
  const android = isAndroid();

  useEffect(() => {
    if (!cwd || view !== 'chat') return;
    void refreshArtifactsFromDisk();
    const id = setInterval(() => {
      // Skip the round-trip while the window is minimized/backgrounded —
      // nothing's watching, and it just spends IPC + disk I/O for no reason.
      if (document.hidden) return;
      void refreshArtifactsFromDisk();
      void pruneMissingArtifacts();
    }, POLL_INTERVAL_MS);
    return () => clearInterval(id);
  }, [cwd, view, refreshArtifactsFromDisk, pruneMissingArtifacts]);

  return (
    <aside className="artifacts-pane">
      <div className="artifacts-head">
        <span>Artifacts ({artifacts.length})</span>
        {/* Android has no folder to open: the chat folder lives in the app's
            private data directory, which neither Explorer's equivalent nor
            any other app can see into. Files leave via "Download" instead. */}
        {cwd && !android && (
          <button
            className="link"
            title="Open this session's working folder in Explorer"
            onClick={() => void ipc.openPath(cwd)}
          >
            Open folder
          </button>
        )}
        {onClose && (
          <button className="artifacts-close" onClick={onClose} aria-label="Close artifacts">
            ✕
          </button>
        )}
      </div>
      {artifacts.length === 0 ? (
        <p className="muted" style={{ fontSize: 14, padding: '4px 8px' }}>
          Files the agent creates or edits will appear here.
        </p>
      ) : (
        <div className="artifacts-list">
          {artifacts.map((a) => (
            <ArtifactCard key={a.path} artifact={a} android={android} />
          ))}
        </div>
      )}
    </aside>
  );
}

function ArtifactCard({ artifact, android }: { artifact: Artifact; android: boolean }) {
  const [error, setError] = useState<string | null>(null);

  return (
    <div className="artifact-card">
      <div className="artifact-name" title={artifact.path}>
        <DocumentIcon /> {artifact.name}
      </div>
      {/* The full path is Windows-useful and Android-noise: there, it points
          into an app-private directory the user can't navigate to anyway. */}
      {!android && (
        <div className="artifact-path muted" title={artifact.path}>
          {artifact.path}
        </div>
      )}
      <div className="artifact-actions">
        {android ? (
          // "Open"/"Show in folder"/"Copy path" are all meaningless against an
          // app-private path. Saving a copy out through the system file picker
          // is the only way a file the model wrote reaches the user's device.
          <button
            onClick={() =>
              void ipc
                .downloadFile(artifact.path)
                .then(() => setError(null))
                .catch((e) => setError(String(e)))
            }
          >
            Download
          </button>
        ) : (
          <>
            <button onClick={() => void ipc.openPath(artifact.path)}>Open</button>
            <button onClick={() => void ipc.revealPath(artifact.path)}>Show in folder</button>
            <button
              onClick={() =>
                void navigator.clipboard.writeText(artifact.path).catch(() => {
                  /* clipboard may be unavailable */
                })
              }
            >
              Copy path
            </button>
          </>
        )}
      </div>
      {error && (
        <p className="muted" style={{ fontSize: 14 }} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
