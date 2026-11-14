import { memo, useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { ToolCall } from '@/stores/chatStore';
import { ipc } from '@/lib/ipc';
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

/** A file name for a saved figure, from its title: lower-case words joined
    by hyphens. `download_text` accepts a plain name only, so nothing that
    could read as a path survives. */
export function vizFileName(title: string): string {
  const slug = title
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 60)
    .replace(/-+$/, '');
  return `${slug || 'figure'}.html`;
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

  // Saves the figure as one self-contained HTML file — markup, stylesheet and
  // the theme's colours as they are now — through the app's save dialog. This
  // replaced "Open in new window", which built the same document but handed
  // it to `window.open`, and the webview opens no windows of its own.
  const [saveFailed, setSaveFailed] = useState(false);
  const title = payload?.title ?? label;
  const download = async () => {
    const doc = hostRef.current ? standaloneVizDocument(hostRef.current, title) : '';
    if (!doc) return;
    try {
      await ipc.downloadText(vizFileName(title), doc);
      setSaveFailed(false);
    } catch {
      setSaveFailed(true);
    }
  };

  return (
    <div className="viz-card">
      <div className="viz-card-head">
        <span className="code-lang">{label}</span>
        <div className="code-actions">
          {payload && (
            <button onClick={() => void download()}>
              {saveFailed ? 'Could not save' : 'Download'}
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
