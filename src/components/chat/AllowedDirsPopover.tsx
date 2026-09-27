import { useEffect, useRef, useState } from 'react';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { AllowedDirsList, useAllowedDirs } from './AllowedDirsList';

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
  const { dirs, revoke, error } = useAllowedDirs(sessionId, open);
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
          <AllowedDirsList dirs={dirs} revoke={revoke} error={error} />
        </div>
      )}
    </span>
  );
}
