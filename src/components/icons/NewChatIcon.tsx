/** Speech-bubble-with-plus icon for "New chat" (Round-3 item 7/9). A dedicated
    inline SVG rather than an emoji sequence — compound emoji like "💬+" render
    inconsistently across Segoe UI Emoji versions on Windows. */
export function NewChatIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M1.5 2.5h13v8.5h-8l-3 2.5v-2.5h-2v-8.5Z"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
      />
      <path d="M8 5v4M6 7h4" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
    </svg>
  );
}
