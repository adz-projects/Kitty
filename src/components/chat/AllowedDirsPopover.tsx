import { useEffect, useRef, useState } from 'react';
import { ipc } from '@/lib/ipc';
import type { SessionAllowedDirs } from '@/lib/types';
import { usePopoverPosition } from '@/lib/usePopoverPosition';

/** What this session is allowed to read and write, on hover over the
    working-directory pill.

    It exists because those grants accumulate and are otherwise invisible.
    Setting a second working folder keeps the first (a model mid-task there must
    not silently lose it), and a dropped folder grants its whole subtree for the
    life of the session — both reasonable, neither discoverable. An allowance the
    user cannot see is one they cannot knowingly withdraw, so this is also the
    only place revoking is reachable.

    Hover rather than click, and nothing rendered until then: this is
    reassurance, not a control panel, and it must not compete with the pill's
    own job of showing where you are working. */
export function AllowedDirsPopover({
  sessionId,
  children,
}: {
  /** `null` before the session is created (a blank chat). There is nothing to
      report then, so the pill renders bare rather than offering an empty list. */
  sessionId: string | null;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const [dirs, setDirs] = useState<SessionAllowedDirs | null>(null);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));

  // `usePopoverPosition` places the panel four pixels clear of the pill, and
  // closing on the pill's own `mouseleave` meant that gap closed the popover
  // before the pointer reached it. Not reliably — a fast diagonal crosses it
  // between two mousemove events and works fine — which is the worst version of
  // the bug, because the revoke button appears to work and then intermittently
  // does not. Nothing about that is visible in a screenshot.
  //
  // A short grace period is the standard answer: any re-entry, on the pill or
  // anywhere in the panel, cancels the pending close.
  const closeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cancelClose = () => {
    if (closeTimer.current !== null) {
      clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  };
  const scheduleClose = () => {
    cancelClose();
    closeTimer.current = setTimeout(() => setOpen(false), 220);
  };
  // A timer that fires after unmount would call `setOpen` on a gone component.
  useEffect(() => cancelClose, []);

  // Loaded on open, not on mount: it is a hover detail, and fetching it for
  // every session the user merely looks at would be a request per render.
  useEffect(() => {
    if (!open || !sessionId) return;
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
  }, [open, sessionId]);

  const revoke = async (path: string) => {
    if (!sessionId) return;
    try {
      await ipc.revokeSessionDir(sessionId, path);
      setDirs(await ipc.listSessionAllowedDirs(sessionId));
    } catch {
      // Best-effort: a failed revoke leaves the list as it was, and the next
      // hover re-reads the real state from the daemon rather than trusting
      // anything optimistic we might have done here.
    }
  };

  // The current working directory is already named on the pill itself, and the
  // chat folder is listed separately below — so neither is repeated here.
  const granted = (dirs?.working_dirs ?? []).filter((d) => d !== dirs?.cwd);
  const attached = dirs?.attached_paths ?? [];

  if (!sessionId) return <>{children}</>;

  return (
    <span
      className="allowed-dirs-trigger"
      onMouseEnter={() => {
        cancelClose();
        setOpen(true);
      }}
      onMouseLeave={scheduleClose}
      ref={(el) => {
        (triggerRef as React.MutableRefObject<HTMLElement | null>).current = el;
      }}
    >
      {children}
      {open && dirs && (
        <div
          ref={popoverRef}
          className="mode-popover allowed-dirs-popover"
          style={style}
          // The panel is a DOM descendant of the trigger, so entering it fires
          // no `mouseenter` on the span above — it needs its own, or the grace
          // period would expire while the pointer sits inside the list.
          onMouseEnter={cancelClose}
          onMouseLeave={scheduleClose}
        >
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
          {granted.map((d) => (
            <div className="allowed-dirs-row" key={d}>
              <span className="allowed-dirs-path">{d}</span>
              <button className="link" onClick={() => void revoke(d)} title="Revoke access">
                ×
              </button>
            </div>
          ))}
          {attached.map((d) => (
            <div className="allowed-dirs-row" key={d}>
              <span className="allowed-dirs-path">{d}</span>
              <button className="link" onClick={() => void revoke(d)} title="Revoke access">
                ×
              </button>
            </div>
          ))}
          {granted.length === 0 && attached.length === 0 && (
            <div className="allowed-dirs-row muted">
              Nothing else — just this chat&apos;s folder.
            </div>
          )}
        </div>
      )}
    </span>
  );
}
