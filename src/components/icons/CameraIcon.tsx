/** Camera icon for the composer's screenshot-capture button (Feature 3). A
    dedicated inline SVG, matching the convention set by UploadIcon/NewChatIcon
    rather than an emoji (renders inconsistently across Segoe UI Emoji
    versions on Windows). Body + lens. */
export function CameraIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M2 5.5A1.5 1.5 0 0 1 3.5 4h1.2l.6-1.1A1 1 0 0 1 6.17 2.4h3.66a1 1 0 0 1 .87.5l.6 1.1h1.2A1.5 1.5 0 0 1 14 5.5v6A1.5 1.5 0 0 1 12.5 13h-9A1.5 1.5 0 0 1 2 11.5v-6Z"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="round"
      />
      <circle cx="8" cy="8.2" r="2.4" stroke="currentColor" strokeWidth="1.2" />
    </svg>
  );
}
