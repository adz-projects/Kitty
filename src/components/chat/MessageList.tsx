import { useEffect, useRef } from 'react';
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

export function MessageList({
  messages,
  empty,
  stage,
}: {
  messages: Message[];
  empty: string;
  stage: ProgressStage | null;
}) {
  if (messages.length > VIRTUALIZE_THRESHOLD) {
    return <VirtualList messages={messages} stage={stage} />;
  }
  return <PlainList messages={messages} empty={empty} stage={stage} />;
}

function PlainList({
  messages,
  empty,
  stage,
}: {
  messages: Message[];
  empty: string;
  stage: ProgressStage | null;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // Only auto-scroll on new content if the user was already at (or near)
  // the bottom — otherwise a streaming reply keeps yanking them back down
  // every time they scroll up to reread something.
  const stickToBottomRef = useRef(true);
  const handleScroll = () => {
    const el = ref.current;
    if (!el) return;
    stickToBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_EPSILON;
  };
  useEffect(() => {
    const el = ref.current;
    if (el && stickToBottomRef.current) el.scrollTop = el.scrollHeight;
  }, [messages]);
  return (
    <div
      className={`message-list${messages.length === 0 ? ' empty' : ''}`}
      ref={ref}
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

function VirtualList({ messages, stage }: { messages: Message[]; stage: ProgressStage | null }) {
  const parentRef = useRef<HTMLDivElement>(null);
  const stickToBottomRef = useRef(true);
  const rv = useVirtualizer({
    count: messages.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 90,
    overscan: 10,
  });

  const handleScroll = () => {
    const el = parentRef.current;
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
    <div className="message-list" ref={parentRef} onScroll={handleScroll}>
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
