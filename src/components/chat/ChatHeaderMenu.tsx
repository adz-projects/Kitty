import { useState } from 'react';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { ModeBadge } from './ModeBadge';
import { AdaptivePathwayToggle } from './AdaptivePathwayToggle';

/** Header overflow menu (chat header simplification, UX-simplification Batch
    3) — approval mode (`ModeBadge`) and Adaptive Pathway pause/resume
    (`AdaptivePathwayToggle`) move here instead of competing for space in the
    always-visible header row. Both keep their exact existing behavior
    unchanged; this is just a new container around them, one click away. */
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
          <ModeBadge />
          <AdaptivePathwayToggle />
        </div>
      )}
    </div>
  );
}
