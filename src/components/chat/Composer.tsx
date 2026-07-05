import { useRef, useState } from 'react';
import { useChatStore } from '@/stores/chatStore';

// Pastes larger than this (chat-only mode) collapse into a document attachment.
const PASTE_THRESHOLD = 500;

/** Message composer: Enter sends, Shift+Enter inserts a newline. While a reply
    streams, sending is blocked and a Stop button cancels the turn. In chat-only
    mode, large pastes collapse into an inlined document attachment. */
export function Composer({
  onSend,
  onStop,
  disabled,
}: {
  onSend: (text: string) => void;
  onStop: () => void;
  disabled: boolean;
}) {
  const [text, setText] = useState('');
  const ref = useRef<HTMLTextAreaElement>(null);
  const toolsEnabled = useChatStore((s) => s.toolsEnabled);
  const addPastedText = useChatStore((s) => s.addPastedText);

  const submit = () => {
    const value = text.trim();
    if (!value || disabled) return;
    onSend(value);
    setText('');
    if (ref.current) ref.current.style.height = 'auto';
  };

  return (
    <div className="composer">
      <textarea
        ref={ref}
        rows={1}
        autoFocus
        value={text}
        placeholder="Ask Kitty…"
        onChange={(e) => {
          setText(e.target.value);
          e.target.style.height = 'auto';
          e.target.style.height = `${Math.min(e.target.scrollHeight, 160)}px`;
        }}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            submit();
          }
        }}
        onPaste={(e) => {
          if (toolsEnabled) return; // agentic mode keeps native paste
          const pasted = e.clipboardData.getData('text');
          if (pasted.length > PASTE_THRESHOLD) {
            e.preventDefault();
            addPastedText(pasted);
          }
        }}
      />
      {disabled ? (
        <button onClick={onStop} title="Stop the current response">
          Stop
        </button>
      ) : (
        <button className="primary" onClick={submit} disabled={!text.trim()}>
          Send
        </button>
      )}
    </div>
  );
}
