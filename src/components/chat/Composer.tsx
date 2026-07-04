import { useRef, useState } from 'react';

/** Message composer: Enter sends, Shift+Enter inserts a newline. While a reply
    streams, sending is blocked and a Stop button cancels the turn. */
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
        placeholder="Ask Goose…"
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
