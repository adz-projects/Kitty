import { memo, useMemo, useState } from 'react';
import type { ToolCall } from '@/stores/chatStore';
import { ToolsIcon } from '@/components/icons/ToolsIcon';

function stringify(v: unknown): string {
  if (v == null) return '';
  if (typeof v === 'string') return v;
  try {
    return JSON.stringify(v, null, 2);
  } catch {
    return String(v);
  }
}

/** Collapsible card for an ACP tool call: name + status, expandable to show
    input params and result.

    Memoized, and the (potentially large — up to the 16KB-per-string server
    cap, see `goosed/stream.rs`'s `cap_strings`) `JSON.stringify` pass is
    deferred until the card is actually expanded rather than running on every
    render: a streaming turn re-renders its `ThinkingBox`'s tool-call list on
    every token, and re-stringifying a large capped result each time dropped
    frames on a big shell/file-read output (MINOR_BUGS.md #9). */
export const ToolCallCard = memo(function ToolCallCard({ call }: { call: ToolCall }) {
  const [open, setOpen] = useState(false);
  const input = useMemo(() => (open ? stringify(call.input) : ''), [open, call.input]);
  const output = useMemo(() => (open ? stringify(call.output) : ''), [open, call.output]);
  return (
    <details className="tool-card" onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary>
        <span>
          <ToolsIcon /> {call.title}
        </span>
        <span className="status-badge">{call.status}</span>
      </summary>
      <div className="tool-body">
        {input && (
          <>
            <div className="muted">input</div>
            <pre>{input}</pre>
          </>
        )}
        {output && (
          <>
            <div className="muted">result</div>
            <pre>{output}</pre>
          </>
        )}
      </div>
    </details>
  );
});
