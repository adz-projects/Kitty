import { useState } from 'react';
import { WarningIcon } from '@/components/icons/WarningIcon';

/** Short, plain-language headline per classified `errorType` — the card's
    title, distinct from `summary`'s longer explanation below it. `undefined`
    (unclassified) falls through to a generic title, matching how
    `humanizeChatError` falls through to generic body text. */
const ERROR_TITLE: Record<string, string> = {
  context_exceeded: 'Context limit reached',
  insufficient_credits: 'Insufficient credits',
  auth_failed: 'Authentication failed',
  network_unreachable: "Can't reach provider",
};

/** Humanized error text in a calm card (icon + headline + body + actions),
    plus an optional expandable raw-detail line, and for a classified
    `provider_error` (`errorType`), a type-specific action button. Never uses
    native <details> — this app's WebView2/Chromium build doesn't actually
    hide <details> content when `open` is false, so raw error text would stay
    visible even "collapsed". Explicit state + conditional render instead.
    Shared by the wizard and the chat error banner — the wizard never passes
    `errorType`, so it always gets the generic title and no action button. */
export function ErrorDetail({
  summary,
  raw,
  errorType,
  onNewSession,
  onOpenProviderSettings,
}: {
  summary: string;
  raw?: string;
  errorType?: string;
  onNewSession?: () => void;
  /** Opens Settings → Providers — the right next step for both
      "insufficient_credits" (switch to a provider with credit) and
      "auth_failed" (fix the stored key), so one prop covers both rather
      than two near-identical handlers. */
  onOpenProviderSettings?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const title = (errorType && ERROR_TITLE[errorType]) || 'Something went wrong';
  return (
    <div className="chat-error-card" role="alert">
      <div className="chat-error-card-head">
        <WarningIcon />
        <span className="chat-error-card-title">{title}</span>
      </div>
      <p className="chat-error-card-body">{summary}</p>
      <div className="chat-error-card-actions">
        {errorType === 'context_exceeded' && onNewSession && (
          <button type="button" className="link" onClick={onNewSession}>
            New Session
          </button>
        )}
        {(errorType === 'insufficient_credits' || errorType === 'auth_failed') &&
          onOpenProviderSettings && (
            <button type="button" className="link" onClick={onOpenProviderSettings}>
              {errorType === 'auth_failed' ? 'Check API Key' : 'Switch Provider'}
            </button>
          )}
        {raw && raw !== summary && (
          <button type="button" className="link" onClick={() => setOpen((o) => !o)}>
            {open ? 'Hide details' : 'Show details'}
          </button>
        )}
      </div>
      {open && raw && raw !== summary && <pre>{raw}</pre>}
    </div>
  );
}
