import { useState } from 'react';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { AdaptivePathwayToggle } from './AdaptivePathwayToggle';

/** Header overflow menu (chat header simplification, UX-simplification Batch
    3) — per-session controls that don't need to compete for space in the
    always-visible header row, one click away. */
export function ChatHeaderMenu() {
  const [open, setOpen] = useState(false);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));

  return (
    <div style={{ position: 'relative' }}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        className="status-badge chat-header-menu-trigger"
        onClick={() => setOpen((o) => !o)}
        title="More"
      >
        ⋯
      </button>
      {open && (
        <div ref={popoverRef} className="chat-header-menu" role="menu" style={style}>
          <AdaptivePathwayToggle />
        </div>
      )}
    </div>
  );
}
