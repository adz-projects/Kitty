/** Speech bubble — chat mode (ModeToggle's "Chat" segment, the chat-only
    header pill). Plain bubble, distinct from `NewChatIcon`'s bubble+plus.
    Replaces 💬. */
export function ChatBubbleIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M1.5 2.5h13v8.5h-8l-3 2.5v-2.5h-2v-8.5Z"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
      />
    </svg>
  );
}
