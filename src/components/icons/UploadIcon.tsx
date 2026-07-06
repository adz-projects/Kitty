/** Upload / attach-file icon for the composer's file button (Round-5). A
    dedicated inline SVG rather than an emoji (📎/📤 render inconsistently
    across Segoe UI Emoji versions on Windows) — matches the convention set by
    NewChatIcon/DoubleChevronIcon. A tray with an up-arrow rising out of it. */
export function UploadIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M8 10.5V2.5M8 2.5 5 5.5M8 2.5l3 3"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M2.5 9.5v3a1 1 0 0 0 1 1h9a1 1 0 0 0 1-1v-3"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
