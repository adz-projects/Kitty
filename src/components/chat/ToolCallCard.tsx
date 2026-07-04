import type { ToolCall } from '@/stores/chatStore';

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
    input params and result. */
export function ToolCallCard({ call }: { call: ToolCall }) {
  const input = stringify(call.input);
  const output = stringify(call.output);
  return (
    <details className="tool-card">
      <summary>
        <span>🔧 {call.title}</span>
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
}
