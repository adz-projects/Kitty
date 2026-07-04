import { useState } from 'react';
import { useChatStore } from '@/stores/chatStore';

const LABELS: Record<string, string> = {
  auto: 'Auto-approve',
  approve: 'Ask every tool',
  smart_approve: 'Smart approve',
};

/** Current approval-mode badge with a click-to-switch popover (CLAUDE.md Phase 3:
    the mode must be visible in the chat UI, not buried in settings). */
export function ModeBadge() {
  const mode = useChatStore((s) => s.mode);
  const availableModes = useChatStore((s) => s.availableModes);
  const setMode = useChatStore((s) => s.setMode);
  const [open, setOpen] = useState(false);

  if (!mode) return null;
  const label = LABELS[mode] ?? mode;

  return (
    <div style={{ position: 'relative' }}>
      <button
        className="status-badge"
        onClick={() => setOpen((o) => !o)}
        title="Approval mode — click to change"
      >
        🛡 {label} ▾
      </button>
      {open && availableModes.length > 0 && (
        <div className="mode-popover" role="menu">
          {availableModes.map((m) => (
            <button
              key={m.id}
              role="menuitemradio"
              aria-checked={m.id === mode}
              className={m.id === mode ? 'active' : ''}
              title={m.description}
              onClick={() => {
                void setMode(m.id);
                setOpen(false);
              }}
            >
              {LABELS[m.id] ?? m.name}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
