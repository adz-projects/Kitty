/** Picture frame — image-kind attachment chips. Replaces 🖼. */
export function ImageIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <rect
        x="1.75"
        y="2.75"
        width="12.5"
        height="10.5"
        rx="1"
        stroke="currentColor"
        strokeWidth="1.2"
      />
      <circle cx="5.5" cy="6" r="1.1" stroke="currentColor" strokeWidth="1.1" />
      <path
        d="m2.5 11.5 3.3-3.3a1 1 0 0 1 1.4 0l1.6 1.6M9.8 9l1-1a1 1 0 0 1 1.4 0l1.3 1.3"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
