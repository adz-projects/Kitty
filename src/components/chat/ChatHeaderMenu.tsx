import { useState } from 'react';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { isAndroid } from '@/lib/platform';
import { useChatStore } from '@/stores/chatStore';
import { Dialog } from '@/components/shared/Dialog';
import { AdaptivePathwayToggle } from './AdaptivePathwayToggle';
import { AllowedDirsList, useAllowedDirs } from './AllowedDirsList';

/** Header overflow menu (chat header simplification, UX-simplification Batch
    3) — per-session controls that don't need to compete for space in the
    always-visible header row, one click away.

    On Android it is the only place for them (#79): incognito, and the
    folders this chat can read and write — which desktop shows on hover over
    the folder pill, and a phone has neither the pill nor hover. */
export function ChatHeaderMenu() {
  const [open, setOpen] = useState(false);
  const [foldersOpen, setFoldersOpen] = useState(false);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));
  const sessionId = useChatStore((s) => s.sessionId);
  const android = isAndroid();

  return (
    <div style={{ position: 'relative' }}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        className="status-badge chat-header-menu-trigger"
        onClick={() => setOpen((o) => !o)}
        title="More"
        aria-label="More chat options"
      >
        ⋯
      </button>
      {open && (
        <div ref={popoverRef} className="chat-header-menu" role="menu" style={style}>
          <AdaptivePathwayToggle />
          {android && sessionId && (
            <button
              className="status-badge"
              role="menuitem"
              onClick={() => {
                setOpen(false);
                setFoldersOpen(true);
              }}
            >
              Allowed folders
            </button>
          )}
        </div>
      )}
      {foldersOpen && sessionId && (
        <AllowedFoldersDialog sessionId={sessionId} onClose={() => setFoldersOpen(false)} />
      )}
    </div>
  );
}

function AllowedFoldersDialog({ sessionId, onClose }: { sessionId: string; onClose: () => void }) {
  const { dirs, revoke, error } = useAllowedDirs(sessionId, true);
  return (
    <Dialog title="Allowed folders" onClose={onClose}>
      <div className="allowed-dirs-dialog">
        {dirs ? (
          <AllowedDirsList dirs={dirs} revoke={revoke} error={error} />
        ) : (
          <p className="muted">Loading…</p>
        )}
      </div>
      <div className="modal-actions">
        <button onClick={onClose}>Done</button>
      </div>
    </Dialog>
  );
}
