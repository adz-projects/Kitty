/** Double-chevron icon (Expand = up, Hide = down — the visual opposite of
    Expand). One SVG, rotated 180° for the down variant. */
export function DoubleChevronIcon({ direction }: { direction: 'up' | 'down' }) {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 16 16"
      fill="currentColor"
      aria-hidden="true"
      style={direction === 'down' ? { transform: 'rotate(180deg)' } : undefined}
    >
      <path d="M7.646 2.146a.5.5 0 0 1 .708 0l6 6a.5.5 0 0 1-.708.708L8 3.207 2.354 8.854a.5.5 0 1 1-.708-.708l6-6z" />
      <path d="M7.646 6.146a.5.5 0 0 1 .708 0l6 6a.5.5 0 0 1-.708.708L8 7.207l-5.646 5.647a.5.5 0 0 1-.708-.708l6-6z" />
    </svg>
  );
}
