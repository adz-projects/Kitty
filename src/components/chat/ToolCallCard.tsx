import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
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

/** Shape produced by MCP tools that want their result rendered as an iframe
    instead of raw JSON/text — currently the `visualizations` server's
    `generate_accessible_table`/`generate_accessible_svg` tools (see
    `plugins/visualizations/visualizations.py`'s `render_config` +
    `html_payload` fields). Parsed defensively: any tool result that doesn't
    match this exact shape falls back to the plain `<pre>` rendering below. */
interface IframeRenderPayload {
  title?: string;
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
    sandbox: typeof rc.sandbox === 'string' ? rc.sandbox : 'allow-scripts',
    html,
  };
}

/** Sandboxed iframe rendering a tool-produced standalone HTML document
    (`srcDoc`), auto-sized via the `mcp-iframe-resize` postMessage the
    document posts on load/resize (see `wrap_in_standalone_html` in
    `plugins/visualizations/visualizations.py`). Only trusts resize messages
    that actually originate from this iframe's own content window. */
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
  const iframePayload = useMemo(() => (open ? parseIframePayload(call.output) : null), [open, call.output]);
  const output = useMemo(
    () => (open && !iframePayload ? stringify(call.output) : ''),
    [open, iframePayload, call.output]
  );
  const onToggle = useCallback((e: React.SyntheticEvent<HTMLDetailsElement>) => {
    setOpen(e.currentTarget.open);
  }, []);
  return (
    <details className="tool-card" onToggle={onToggle}>
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
        {iframePayload && (
          <>
            <div className="muted">result</div>
            <ToolResultIframe payload={iframePayload} />
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
