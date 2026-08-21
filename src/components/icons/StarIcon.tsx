/** Star — the "default for new sessions" marker on a provider card, and the
    action that sets it.
 *
    Filled means *is* the default; outline means *make* it the default. Same
    glyph either way on purpose: the outline button and the filled marker are
    the two halves of one idea, and drawing them as different shapes would hide
    that the button produces the mark. Icon-only, paired with a `title` tooltip
    by the caller. */
export function StarIcon({ filled = true }: { filled?: boolean }) {
  const d = 'M8 2.2 9.7 6l4.1.4-3.1 2.8.9 4-3.6-2.1-3.6 2.1.9-4-3.1-2.8L6.3 6 8 2.2Z';
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d={d}
        fill={filled ? 'currentColor' : 'none'}
        stroke={filled ? 'none' : 'currentColor'}
        strokeWidth={filled ? undefined : 1.2}
        strokeLinejoin={filled ? undefined : 'round'}
      />
    </svg>
  );
}
