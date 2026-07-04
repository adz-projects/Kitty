import { useEffect, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { MessageItem } from './MessageItem';
import type { Message } from '@/stores/chatStore';

// Below this many messages we render plainly (the well-tested path); beyond it
// we virtualize so a 500+ message session scrolls smoothly (Phase 8).
const VIRTUALIZE_THRESHOLD = 200;

export function MessageList({
  messages,
  empty,
  typing,
}: {
  messages: Message[];
  empty: string;
  typing: boolean;
}) {
  if (messages.length > VIRTUALIZE_THRESHOLD) {
    return <VirtualList messages={messages} typing={typing} />;
  }
  return <PlainList messages={messages} empty={empty} typing={typing} />;
}

function PlainList({
  messages,
  empty,
  typing,
}: {
  messages: Message[];
  empty: string;
  typing: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages]);
  return (
    <div className="message-list" ref={ref}>
      {messages.length === 0 && <p className="muted">{empty}</p>}
      {messages.map((m) => (
        <MessageItem key={m.id} message={m} />
      ))}
      {typing && <span className="typing">Thinking…</span>}
    </div>
  );
}

function VirtualList({ messages, typing }: { messages: Message[]; typing: boolean }) {
  const parentRef = useRef<HTMLDivElement>(null);
  const rv = useVirtualizer({
    count: messages.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 90,
    overscan: 10,
  });

  // Keep pinned to the newest content as messages stream/append.
  useEffect(() => {
    if (messages.length > 0) rv.scrollToIndex(messages.length - 1, { align: 'end' });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [messages]);

  return (
    <div className="message-list" ref={parentRef}>
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
            <MessageItem message={messages[vi.index]} />
          </div>
        ))}
      </div>
      {typing && <span className="typing">Thinking…</span>}
    </div>
  );
}
