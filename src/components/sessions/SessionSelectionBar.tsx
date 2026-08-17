import { TrashIcon } from '@/components/icons/TrashIcon';
import { ExportIcon } from '@/components/icons/ExportIcon';

/** Bottom action bar shown while the saved-chats list (Android) is in bulk
    selection mode (release-fixes items 8-10, long-press to enter). Sticky
    like `MobileTabBar`, styled the same way, so it reads as part of the same
    bottom-of-screen chrome rather than a new UI pattern. */
export function SessionSelectionBar({
  count,
  busy,
  error,
  onDelete,
  onExport,
  onCancel,
}: {
  count: number;
  busy: boolean;
  error: string | null;
  onDelete: () => void;
  /** Undefined hides the button entirely — bulk export doesn't actually
      work on Android yet (writeFile there can't resolve a plain folder +
      filename path the way desktop does), so it's hidden rather than
      shipped broken until that's fixed. */
  onExport?: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="session-selection-bar">
      <div className="row">
        <button onClick={onCancel} disabled={busy} title="Cancel selection" aria-label="Cancel selection">
          ✕
        </button>
        <span className="muted">{count} selected</span>
        {onExport && (
          <button
            onClick={onExport}
            disabled={busy || count === 0}
            title="Export selected chats"
            aria-label="Export selected chats"
          >
            <ExportIcon />
          </button>
        )}
        <button
          onClick={onDelete}
          disabled={busy || count === 0}
          title="Delete selected chats"
          aria-label="Delete selected chats"
        >
          <TrashIcon />
        </button>
      </div>
      {error && (
        <div className="chat-error" role="alert">
          {error}
        </div>
      )}
    </div>
  );
}
