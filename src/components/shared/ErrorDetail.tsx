import { useState } from 'react';

/** Humanized error text plus an optional expandable raw-detail line, and for
    a classified `provider_error` (`errorType`), a type-specific action
    button. Never uses native <details> — this app's WebView2/Chromium build
    doesn't actually hide <details> content when `open` is false, so raw
    error text would stay visible even "collapsed". Explicit state +
    conditional render instead. Shared by the wizard and the chat error
    banner — the wizard never passes `errorType`, so the action button never
    renders there. */
export function ErrorDetail({
  summary,
  raw,
  errorType,
  onNewSession,
  onSwitchProvider,
}: {
  summary: string;
  raw?: string;
  errorType?: string;
  onNewSession?: () => void;
  onSwitchProvider?: () => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className="error-detail">
      <p className="muted" style={{ margin: 0, color: 'var(--danger)' }}>
        {summary}
      </p>
      {errorType === 'context_exceeded' && onNewSession && (
        <button
          type="button"
          className="link"
          style={{ color: 'var(--accent)' }}
          onClick={onNewSession}
        >
          New Session
        </button>
      )}
      {errorType === 'insufficient_credits' && onSwitchProvider && (
        <button
          type="button"
          className="link"
          style={{ color: 'var(--accent)' }}
          onClick={onSwitchProvider}
        >
          Switch Provider
        </button>
      )}
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
