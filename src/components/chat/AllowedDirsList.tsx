import { useEffect, useState } from 'react';
import { ipc } from '@/lib/ipc';
import { useChatStore } from '@/stores/chatStore';
import type { SessionAllowedDirs } from '@/lib/types';

/** What a chat may read and write, loaded while `active`, with revoke. */
export function useAllowedDirs(sessionId: string | null, active: boolean) {
  const [dirs, setDirs] = useState<SessionAllowedDirs | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Loaded when shown, not on mount: fetching it for every chat the user
  // merely looks at would be a request per render.
  useEffect(() => {
    if (!active || !sessionId) return;
    let cancelled = false;
    void ipc
      .listSessionAllowedDirs(sessionId)
      .then((d) => {
        if (!cancelled) setDirs(d);
      })
      .catch(() => {
        if (!cancelled) setDirs(null);
      });
    return () => {
      cancelled = true;
    };
  }, [active, sessionId]);

  const revoke = async (path: string) => {
    if (!sessionId) return;
    setError(null);
    try {
      await ipc.revokeSessionDir(sessionId, path);
      setDirs(await ipc.listSessionAllowedDirs(sessionId));
      // The chat's cached grants too, or the revoked folder would keep
      // scoping the artifacts pane (#38).
      await useChatStore.getState().refreshSessionGrants();
    } catch (e) {
      setError(`Couldn't revoke access: ${String(e)}`);
    }
  };

  return { dirs, revoke, error };
}

/** The rows: the working folder, the chat folder, then everything else the
    chat was given (each revocable). */
export function AllowedDirsList({
  dirs,
  revoke,
  error,
}: {
  dirs: SessionAllowedDirs;
  revoke: (path: string) => Promise<void>;
  error: string | null;
}) {
  // The current working directory and the chat folder are the chat's own;
  // everything else was granted along the way.
  const granted = dirs.working_dirs.filter((d) => d !== dirs.cwd);
  const attached = dirs.attached_paths;
  return (
    <>
      <div className="allowed-dirs-heading muted">This chat can read and write</div>
      {dirs.cwd && (
        <div className="allowed-dirs-row">
          <span className="allowed-dirs-path">{dirs.cwd}</span>
          <span className="muted">working folder</span>
        </div>
      )}
      {dirs.chat_dir && dirs.chat_dir !== dirs.cwd && (
        <div className="allowed-dirs-row">
          <span className="allowed-dirs-path">{dirs.chat_dir}</span>
          <span className="muted">chat folder</span>
        </div>
      )}
      {[...granted, ...attached].map((d) => (
        <div className="allowed-dirs-row" key={d}>
          <span className="allowed-dirs-path">{d}</span>
          <button
            className="link"
            onClick={() => void revoke(d)}
            title="Revoke access"
            aria-label={`Revoke access to ${d}`}
          >
            ×
          </button>
        </div>
      ))}
      {granted.length === 0 && attached.length === 0 && (
        <div className="allowed-dirs-row muted">Nothing else — just this chat&apos;s folder.</div>
      )}
      {error && <div className="allowed-dirs-row error">{error}</div>}
    </>
  );
}
