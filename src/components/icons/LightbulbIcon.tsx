/** Lightbulb — Adaptive Pathway hints/suggestions (hint summary, hint badge,
    hints-on toggle, "explore an alternative" feedback). Replaces 💡, which
    renders inconsistently across Segoe UI Emoji versions on Windows. */
export function LightbulbIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M8 1.5a4.5 4.5 0 0 0-2.5 8.25c.35.24.5.6.5 1v.25h4v-.25c0-.4.15-.76.5-1A4.5 4.5 0 0 0 8 1.5Z"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
      />
      <path
        d="M6.25 13.25h3.5M6.75 14.5h2.5"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
      />
    </svg>
  );
}
