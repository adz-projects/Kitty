import { memo, useEffect, useMemo, useRef, useState } from 'react';
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

/** Shape produced by MCP tools that want their result rendered as an iframe
    instead of raw JSON/text — currently `kitty-tools`'s
    `generate_accessible_table`/`generate_accessible_svg`/
    `generate_accessible_chart` tools (see `plugins/kitty-tools/src/tools/viz/
    mod.rs`'s `success_payload`, which builds the `render_config` +
    `html_payload` fields). Parsed defensively: any tool result that doesn't
    match this exact shape falls back to plain text below. */
interface IframeRenderPayload {
  title?: string;
  /** The iframe `sandbox` token list is FIXED here, not taken from the tool
      payload — a crafted or buggy tool could otherwise pass `allow-scripts
      allow-same-origin` (treating the srcDoc as same-origin with the app) or
      `allow-top-navigation` (steering the whole window). `allow-scripts` alone
      keeps the visualization's own JS working while it stays fully opaque. */
  sandbox: string;
  html: string;
}

function parseIframePayload(output: unknown): IframeRenderPayload | null {
  if (typeof output !== 'string') return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(output);
  } catch {
    return null;
  }
  if (typeof parsed !== 'object' || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  const renderConfig = obj.render_config;
  if (typeof renderConfig !== 'object' || renderConfig === null) return null;
  const rc = renderConfig as Record<string, unknown>;
  if (rc.target !== 'iframe') return null;
  const html = obj.html_payload;
  if (typeof html !== 'string' || !html.trim()) return null;
  return {
    title: typeof rc.title === 'string' ? rc.title : undefined,
    sandbox: 'allow-scripts',
    html,
  };
}

const VIZ_LABELS: Record<string, string> = {
  generate_accessible_svg: 'Diagram',
  generate_accessible_table: 'Table',
  generate_accessible_chart: 'Chart',
  generate_accessible_mermaid: 'Mermaid',
};

/** Sandboxed iframe rendering a tool-produced standalone HTML document
    (`srcDoc`), auto-sized via the `mcp-iframe-resize` postMessage the
    document posts on load/resize (see `wrap_in_standalone_html` in
    `plugins/kitty-tools/src/tools/viz/mod.rs`, and the resize script baked
    into `assets/wrapper.html`). Only trusts resize messages that actually
    originate from this iframe's own content window. */
function ToolResultIframe({ payload }: { payload: IframeRenderPayload }) {
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const [height, setHeight] = useState(120);

  useEffect(() => {
    function onMessage(e: MessageEvent) {
      if (e.source !== iframeRef.current?.contentWindow) return;
      const data = e.data as { type?: string; height?: number } | null;
      if (data?.type === 'mcp-iframe-resize' && typeof data.height === 'number') {
        setHeight(Math.max(40, Math.min(data.height, 4000)));
      }
    }
    window.addEventListener('message', onMessage);
    return () => window.removeEventListener('message', onMessage);
  }, []);

  return (
    <iframe
      ref={iframeRef}
      className="tool-result-iframe"
      title={payload.title ?? 'Tool visualization'}
      srcDoc={payload.html}
      sandbox={payload.sandbox}
      style={{ height }}
    />
  );
}

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
  const url = URL.createObjectURL(new Blob([html], { type: 'text/html' }));
  window.open(url, '_blank');
  // Revoked after a delay rather than immediately — the new tab needs the
  // blob to still resolve when it actually loads the URL.
  revokeLater(url);
}

/** A viz tool didn't hand back a parseable iframe envelope. Distinguish the
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
    for, not a debugging aside. */
export const VisualizationCard = memo(function VisualizationCard({ call }: { call: ToolCall }) {
  const payload = useMemo(() => parseIframePayload(call.output), [call.output]);
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

  return (
    <div className="viz-card">
      <div className="viz-card-head">
        <span className="code-lang">{label}</span>
        <div className="code-actions">
          {payload && (
            <button onClick={() => openInNewWindow(payload.html)}>Open in new window</button>
          )}
        </div>
      </div>
      <div className="viz-card-body">
        {payload && <ToolResultIframe payload={payload} />}
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
