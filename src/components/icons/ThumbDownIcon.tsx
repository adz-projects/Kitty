/** Thumbs down — "don't suggest this again" hint feedback. `ThumbUpIcon`
    flipped vertically. Replaces 👎. */
export function ThumbDownIcon() {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      style={{ transform: 'scaleY(-1)' }}
    >
      <path
        d="M2 7h2.2v6H2a1 1 0 0 1-1-1V8a1 1 0 0 1 1-1Z"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="round"
      />
      <path
        d="M4.2 7l2.6-4.6a1.2 1.2 0 0 1 2.2.7v2.4h3.2a1.2 1.2 0 0 1 1.15 1.55l-1.2 4.2A1.2 1.2 0 0 1 11 12.2H4.2V7Z"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="round"
      />
    </svg>
  );
}
