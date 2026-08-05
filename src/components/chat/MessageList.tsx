import { useEffect, useRef, type MutableRefObject } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { MessageItem } from './MessageItem';
import { ThinkingIndicator, type ProgressStage } from './ThinkingIndicator';
import type { Message } from '@/stores/chatStore';

// Below this many messages we render plainly (the well-tested path); beyond it
// we virtualize so a 500+ message session scrolls smoothly (Phase 8).
const VIRTUALIZE_THRESHOLD = 200;

// How close to the bottom (px) still counts as "at the bottom" for
// auto-scroll purposes — a streaming reply grows the scroll height every
// few tokens, so an exact `=== ` check would drop stickiness on the very
// next delta.
const BOTTOM_EPSILON = 48;

/** The shared scroll-container element (ref) and a snapshot of its scrollTop
    carried across the PlainList<->VirtualList switch (WS8). Both lists receive
    them so crossing VIRTUALIZE_THRESHOLD mid-stream doesn't lose the user's
    scroll position (or, if they were at the bottom, yank them somewhere) when
    one component unmounts and the other mounts in its place. */
export interface ScrollCarry {
  scrollRef: MutableRefObject<HTMLDivElement | null>;
  savedScrollRef: MutableRefObject<number | null>;
}

export function MessageList({
  messages,
  empty,
  stage,
}: {
  messages: Message[];
  empty: string;
  stage: ProgressStage | null;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const savedScrollRef = useRef<number | null>(null);
  const carry: ScrollCarry = { scrollRef, savedScrollRef };
  if (messages.length > VIRTUALIZE_THRESHOLD) {
    return <VirtualList messages={messages} stage={stage} carry={carry} />;
  }
  return <PlainList messages={messages} empty={empty} stage={stage} carry={carry} />;
}

/** Snapshot current scrollTop before unmount and restore it on mount, so the
    PlainList<->VirtualList switch preserves position. Runs its cleanup on
    unmount and its restore on the first mount only ([] deps). */
function useScrollCarry(
  { scrollRef, savedScrollRef }: ScrollCarry,
  stickToBottomRef: MutableRefObject<boolean>
) {
  useEffect(() => {
    const el = scrollRef.current;
    if (el && savedScrollRef.current != null) {
      el.scrollTop = savedScrollRef.current;
      savedScrollRef.current = null;
      // Re-derive stickiness from the restored position, not the fresh `true`
      // default — else the next auto-scroll yanks a scrolled-up reader down.
      stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_EPSILON;
    }
    // Snapshot the element for the cleanup — by the time the cleanup runs
    // (component type switch), `scrollRef.current` may point elsewhere.
    const elForCleanup = scrollRef.current;
    return () => {
      if (elForCleanup) savedScrollRef.current = elForCleanup.scrollTop;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}

function PlainList({
  messages,
  empty,
  stage,
  carry,
}: {
  messages: Message[];
  empty: string;
  stage: ProgressStage | null;
  carry: ScrollCarry;
}) {
  const { scrollRef } = carry;
  // Only auto-scroll on new content if the user was already at (or near)
  // the bottom — otherwise a streaming reply keeps yanking them back down
  // every time they scroll up to reread something.
  const stickToBottomRef = useRef(true);
  useScrollCarry(carry, stickToBottomRef);
  const handleScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_EPSILON;
  };
  useEffect(() => {
    const el = scrollRef.current;
    if (el && stickToBottomRef.current) el.scrollTop = el.scrollHeight;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [messages]);
  return (
    <div
      className={`message-list${messages.length === 0 ? ' empty' : ''}`}
      ref={scrollRef}
      onScroll={handleScroll}
    >
      {messages.length === 0 && <p className="muted">{empty}</p>}
      {messages.map((m, i) => (
        <MessageItem key={m.id} message={m} index={i} />
      ))}
      {stage && <ThinkingIndicator stage={stage} />}
    </div>
  );
}

function VirtualList({
  messages,
  stage,
  carry,
}: {
  messages: Message[];
  stage: ProgressStage | null;
  carry: ScrollCarry;
}) {
  const { scrollRef } = carry;
  const stickToBottomRef = useRef(true);
  useScrollCarry(carry, stickToBottomRef);
  const rv = useVirtualizer({
    count: messages.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 90,
    overscan: 10,
  });

  const handleScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_EPSILON;
  };

  // Keep pinned to the newest content as messages stream/append — but only
  // while the user is already at the bottom (see PlainList's same check).
  useEffect(() => {
    if (messages.length > 0 && stickToBottomRef.current) {
      rv.scrollToIndex(messages.length - 1, { align: 'end' });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [messages]);

  return (
    <div className="message-list" ref={scrollRef} onScroll={handleScroll}>
      <div style={{ height: rv.getTotalSize(), position: 'relative', width: '100%' }}>
        {rv.getVirtualItems().map((vi) => (
          <div
            key={messages[vi.index].id}
            data-index={vi.index}
            ref={rv.measureElement}
            style={{
              position: 'absolute',
              top: 0,
              left: 0,
              width: '100%',
              transform: `translateY(${vi.start}px)`,
            }}
          >
            <MessageItem message={messages[vi.index]} index={vi.index} />
          </div>
        ))}
      </div>
      {stage && <ThinkingIndicator stage={stage} />}
    </div>
  );
}
