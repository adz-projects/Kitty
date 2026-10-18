import { useEffect, useState, type KeyboardEvent } from 'react';
import { useChatStore } from '@/stores/chatStore';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { effortLabel, selectedEffort } from '@/lib/effort';
import { SparkIcon, SparkOffIcon } from '@/components/icons/SparkIcon';
import type { EffortOption } from '@/lib/types';

/** The sparks for one level: Off is the struck-through outline, level i of
    the ladder is i filled sparks. */
function Sparks({ option }: { option: EffortOption }) {
  if (option.stars === 0) return <SparkOffIcon />;
  return (
    <span className="effort-sparks">
      {Array.from({ length: option.stars }, (_, i) => (
        <SparkIcon key={i} />
      ))}
    </span>
  );
}

/** Reasoning-effort picker for the chat's own card. Levels are normalized to
    sparks — Off, then one to N for the model's N levels, lowest first — so
    every model reads the same way however its provider names the levels; the
    name is in the tooltip and the accessible label.

    A custom popover rather than a `<select>`: a native option can't draw an
    SVG, and a Unicode star renders differently (or as an emoji) across
    Windows and Android fonts. Renders nothing when the chat's model has no
    effort control (`thinkingEffort` is `null`). */
export function EffortPicker() {
  const thinkingEffort = useChatStore((s) => s.thinkingEffort);
  const setThinkingEffort = useChatStore((s) => s.setThinkingEffort);
  // While a new chat is being created this still shows the outgoing chat's
  // value; disable it rather than set effort on a session that doesn't exist.
  const creatingSession = useChatStore((s) => s.creatingSession);
  const [open, setOpen] = useState(false);
  const { triggerRef, popoverRef, style } = usePopoverPosition(open, () => setOpen(false));

  // Focus the selected row on open, so arrow keys start from it.
  useEffect(() => {
    if (!open) return;
    const active = popoverRef.current?.querySelector<HTMLButtonElement>('[aria-checked="true"]');
    (active ?? popoverRef.current?.querySelector<HTMLButtonElement>('button'))?.focus();
  }, [open, popoverRef]);

  if (!thinkingEffort) return null;
  const selected = selectedEffort(thinkingEffort);
  if (!selected) return null;
  const max = thinkingEffort.max_stars;

  const choose = (value: string) => {
    setOpen(false);
    (triggerRef.current as HTMLButtonElement | null)?.focus();
    if (value !== thinkingEffort.current_value) void setThinkingEffort(value);
  };

  const onMenuKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp' && e.key !== 'Home' && e.key !== 'End') return;
    e.preventDefault();
    const items = Array.from(
      popoverRef.current?.querySelectorAll<HTMLButtonElement>('[role="menuitemradio"]') ?? []
    );
    if (items.length === 0) return;
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    const next =
      e.key === 'Home'
        ? 0
        : e.key === 'End'
          ? items.length - 1
          : (at + (e.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length;
    items[next]?.focus();
  };

  return (
    <div style={{ position: 'relative' }}>
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        className="status-badge effort-picker"
        title={effortLabel(selected, max)}
        aria-label={effortLabel(selected, max)}
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={creatingSession}
        onClick={() => setOpen((o) => !o)}
      >
        <Sparks option={selected} />
      </button>
      {open && (
        <div
          ref={popoverRef}
          className="mode-popover effort-popover"
          role="menu"
          aria-label="Reasoning effort"
          style={style}
          onKeyDown={onMenuKey}
        >
          {thinkingEffort.options.map((o) => {
            const checked = o.value === selected.value;
            return (
              <button
                key={o.value}
                role="menuitemradio"
                aria-checked={checked}
                aria-label={effortLabel(o, max)}
                title={effortLabel(o, max)}
                className={`effort-option${checked ? ' active' : ''}`}
                onClick={() => choose(o.value)}
              >
                <Sparks option={o} />
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
