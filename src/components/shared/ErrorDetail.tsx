import { useState } from 'react';

/** Humanized error text plus an optional expandable raw-detail line. Never
    uses native <details> — this app's WebView2/Chromium build doesn't
    actually hide <details> content when `open` is false, so raw error text
    would stay visible even "collapsed". Explicit state + conditional render
    instead. Shared by the wizard and the chat error banner. */
export function ErrorDetail({ summary, raw }: { summary: string; raw?: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="error-detail">
      <p className="muted" style={{ margin: 0, color: 'var(--danger)' }}>
        {summary}
      </p>
      {raw && raw !== summary && (
        <>
          <button type="button" className="link" onClick={() => setOpen((o) => !o)}>
            {open ? 'Hide details' : 'Show details'}
          </button>
          {open && <pre>{raw}</pre>}
        </>
      )}
    </div>
  );
}
