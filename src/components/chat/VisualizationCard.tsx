import { memo, useLayoutEffect, useMemo, useRef } from 'react';
import type { ToolCall } from '@/stores/chatStore';
import { mountViz, parseVizPayload, standaloneVizDocument } from '@/lib/viz';

function stringify(v: unknown): string {
  if (v == null) return '';
  if (typeof v === 'string') return v;
  try {
    return JSON.stringify(v, null, 2);
  } catch {
    return String(v);
  }
}

const VIZ_LABELS: Record<string, string> = {
  generate_accessible_svg: 'Diagram',
  generate_accessible_table: 'Table',
  generate_accessible_chart: 'Chart',
  generate_accessible_mermaid: 'Mermaid',
};

// Pending blob-URL revokes from `openInNewWindow`, tracked so they can be
// cancelled at teardown (multiple visualization cards → several 30s timers
// would otherwise outlive the window).
const revokeTimers = new Set<ReturnType<typeof setTimeout>>();
const revokeLater = (url: string) => {
  const id = setTimeout(() => {
    revokeTimers.delete(id);
    URL.revokeObjectURL(url);
  }, 30_000);
  revokeTimers.add(id);
};
if (typeof window !== 'undefined') {
  window.addEventListener('beforeunload', () => {
    revokeTimers.forEach((id) => {
      clearTimeout(id);
      revokeTimers.delete(id);
    });
  });
}

function openInNewWindow(html: string) {
  if (!html) return;
  const url = URL.createObjectURL(new Blob([html], { type: 'text/html' }));
  window.open(url, '_blank');
  // Revoked after a delay rather than immediately — the new tab needs the
  // blob to still resolve when it actually loads the URL.
  revokeLater(url);
}

/** A viz tool didn't hand back a parseable envelope. Distinguish the
    actionable "payload was cut off" case (truncation marker) from a harder
    failure, so we never dump a giant raw JSON blob for what should be a
    diagram. */
function truncationMessage(output: unknown): string | null {
  if (typeof output === 'string' && /…\[truncated \d+ bytes\]/.test(output)) {
    return 'The diagram output was cut off because it was too large to pass through. Try a smaller or simpler diagram.';
  }
  return null;
}

/** Inline visualization result, rendered directly in the message flow the
    same way a fenced code block is: always mounted, no expand click needed.
    Distinct from `ToolCallCard` (which stays collapsed-by-default for every
    other tool) because a diagram/table/chart *is* the answer the user asked
    for, not a debugging aside.

    The figure is mounted in a shadow root on a plain element of the chat —
    see `lib/viz.ts` for why that replaced the sandboxed iframe. The mount
    happens in a layout effect, so the figure is in the first painted frame at
    its final size: nothing is measured and nothing resizes afterwards. */
export const VisualizationCard = memo(function VisualizationCard({ call }: { call: ToolCall }) {
  const payload = useMemo(() => parseVizPayload(call.output), [call.output]);
  const truncMsg = useMemo(
    () => (payload ? null : truncationMessage(call.output)),
    [payload, call.output]
  );
  const outputText = useMemo(
    () => (payload || truncMsg ? '' : stringify(call.output)),
    [payload, truncMsg, call.output]
  );
  const label = (call.toolName && VIZ_LABELS[call.toolName]) || call.title || 'Visualization';
  const isSettled = call.status !== 'pending' && call.status !== 'running';

  const hostRef = useRef<HTMLDivElement>(null);
  const html = payload?.html;
  useLayoutEffect(() => {
    if (hostRef.current && html) mountViz(hostRef.current, html);
  }, [html]);

  return (
    <div className="viz-card">
      <div className="viz-card-head">
        <span className="code-lang">{label}</span>
        <div className="code-actions">
          {payload && (
            <button
              onClick={() => {
                if (hostRef.current) {
                  openInNewWindow(standaloneVizDocument(hostRef.current, payload.title ?? label));
                }
              }}
            >
              Open in new window
            </button>
          )}
        </div>
      </div>
      <div className="viz-card-body">
        {payload && <div ref={hostRef} className="viz-inline" />}
        {truncMsg && <div className="muted viz-truncated">{truncMsg}</div>}
        {!payload && !truncMsg && outputText && (
          <pre className={call.status === 'failed' ? 'viz-error' : undefined}>{outputText}</pre>
        )}
        {!payload && !outputText && (
          <div className="muted viz-loading">{isSettled ? 'No output.' : 'Rendering…'}</div>
        )}
      </div>
    </div>
  );
});
