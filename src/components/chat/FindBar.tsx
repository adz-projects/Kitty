import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useFindStore } from '@/stores/findStore';
import { findRanges, paintMatches, revealMatch } from '@/lib/findInPage';
import { useChatStore } from '@/stores/chatStore';
import { useBackDismiss } from '@/hooks/useBackDismiss';

/** Find in this chat (Ctrl+F): docked at the bottom of the chat, above the
    composer. Enter / Shift+Enter step through the matches, Escape closes.
    The transcript is searched as rendered (`lib/findInPage.ts`), and found
    again as a reply streams in. */
export function FindBar() {
  const open = useFindStore((s) => s.open);
  const focusTick = useFindStore((s) => s.focusTick);
  const close = useFindStore((s) => s.close);
  const messages = useChatStore((s) => s.messages);
  const [query, setQuery] = useState('');
  const [count, setCount] = useState(0);
  const [current, setCurrent] = useState(0);
  const barRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const rangesRef = useRef<Range[]>([]);
  const currentRef = useRef(0);
  useBackDismiss(open, close);

  const listEl = () => barRef.current?.closest('.chat')?.querySelector('.message-list') ?? null;

  // Search the transcript; keep the current match where it was when the
  // text merely grew (a streaming reply), else start at the first.
  const search = useCallback(
    (reveal: boolean) => {
      const root = listEl();
      const ranges = root ? findRanges(root, query) : [];
      rangesRef.current = ranges;
      const at = Math.min(currentRef.current, Math.max(ranges.length - 1, 0));
      currentRef.current = at;
      setCount(ranges.length);
      setCurrent(at);
      paintMatches(ranges, at);
      if (reveal) revealMatch(ranges[at]);
    },
    [query]
  );

  const step = (delta: number) => {
    const ranges = rangesRef.current;
    if (ranges.length === 0) return;
    const at = (currentRef.current + delta + ranges.length) % ranges.length;
    currentRef.current = at;
    setCurrent(at);
    paintMatches(ranges, at);
    revealMatch(ranges[at]);
  };

  // A new query starts from the first match.
  useEffect(() => {
    if (!open) return;
    currentRef.current = 0;
    search(true);
  }, [open, query, search]);

  // The list re-renders unvirtualized once find opens, and grows while a
  // reply streams: search again after either settles.
  useEffect(() => {
    if (!open) return;
    const root = listEl();
    if (!root) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const observer = new MutationObserver(() => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => search(false), 150);
    });
    observer.observe(root, { childList: true, subtree: true, characterData: true });
    return () => {
      observer.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [open, search, messages]);

  // Open (again): the field takes the keyboard, its text selected.
  useLayoutEffect(() => {
    if (!open) return;
    inputRef.current?.focus();
    inputRef.current?.select();
  }, [open, focusTick]);

  // Closing clears the paint.
  useEffect(() => {
    if (!open) paintMatches([], 0);
    return () => paintMatches([], 0);
  }, [open]);

  if (!open) return null;
  return (
    <div ref={barRef} className="find-bar" role="search" aria-label="Find in this chat">
      <input
        ref={inputRef}
        type="search"
        aria-label="Find in this chat"
        placeholder="Find in this chat"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault();
            step(e.shiftKey ? -1 : 1);
          } else if (e.key === 'Escape') {
            e.preventDefault();
            e.stopPropagation();
            close();
          }
        }}
      />
      <span className="find-count muted" aria-live="polite">
        {query.trim() ? (count ? `${current + 1} of ${count}` : 'No matches') : ''}
      </span>
      <button
        type="button"
        className="icon-button"
        onClick={() => step(-1)}
        disabled={count === 0}
        title="Previous match (Shift+Enter)"
        aria-label="Previous match"
      >
        ↑
      </button>
      <button
        type="button"
        className="icon-button"
        onClick={() => step(1)}
        disabled={count === 0}
        title="Next match (Enter)"
        aria-label="Next match"
      >
        ↓
      </button>
      <button
        type="button"
        className="icon-button"
        onClick={close}
        title="Close (Escape)"
        aria-label="Close find"
      >
        ×
      </button>
    </div>
  );
}
