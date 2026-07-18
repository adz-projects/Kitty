import { useState } from 'react';
import { UNCATEGORIZED, useSessionStore } from '@/stores/sessionStore';
import { usePopoverPosition } from '@/lib/usePopoverPosition';

/** Per-session ⋮ menu (Round-3 item 5) — replaces the bulky per-row folder
    `<select>` + separate delete button. Reuses the same popover pattern as
    ModeBadge/ProviderBadge (`.mode-popover`, absolutely positioned). */
export function SessionKebabMenu({
  sessionId,
  folders,
  current,
  onDelete,
}: {
  sessionId: string;
  folders: string[];
  current: string;
  onDelete: () => void;
}) {
  const [open, setOpen] = useState(false);
  const assignFolder = useSessionStore((s) => s.assignFolder);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));

  return (
    <div style={{ position: 'relative' }} onClick={(e) => e.stopPropagation()}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        className="session-kebab"
        onClick={() => setOpen((o) => !o)}
        title="Session options"
      >
        ⋮
      </button>
      {open && (
        <div ref={popoverRef} className="mode-popover" role="menu" style={style}>
          <span className="muted" style={{ fontSize: 11, padding: '4px 8px' }}>
            Move to folder
          </span>
          <button
            role="menuitemradio"
            aria-checked={current === ''}
            className={current === '' ? 'active' : ''}
            onClick={() => {
              void assignFolder(sessionId, null);
              setOpen(false);
            }}
          >
            {UNCATEGORIZED}
          </button>
          {folders.map((f) => (
            <button
              key={f}
              role="menuitemradio"
              aria-checked={current === f}
              className={current === f ? 'active' : ''}
              onClick={() => {
                void assignFolder(sessionId, f);
                setOpen(false);
              }}
            >
              {f}
            </button>
          ))}
          <hr className="mode-popover-sep" />
          <button
            onClick={() => {
              setOpen(false);
              onDelete();
            }}
          >
            Delete
          </button>
        </div>
      )}
    </div>
  );
}
