/** Forking-path glyph — "Branch a new session from here". */
export function BranchIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="4.5" cy="3" r="1.5" stroke="currentColor" strokeWidth="1.2" />
      <circle cx="4.5" cy="13" r="1.5" stroke="currentColor" strokeWidth="1.2" />
      <circle cx="11.5" cy="5" r="1.5" stroke="currentColor" strokeWidth="1.2" />
      <path
        d="M4.5 4.5v7M11.5 6.5c0 3-7 2.5-7 5"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinecap="round"
      />
    </svg>
  );
}
