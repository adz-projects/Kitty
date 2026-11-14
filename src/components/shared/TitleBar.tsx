import { getCurrentWindow } from '@tauri-apps/api/window';
import { isAndroid } from '@/lib/platform';
import { useChatStore } from '@/stores/chatStore';
import { useRouteStore, type HubView } from '@/stores/routeStore';
import { KittyIcon } from '@/components/icons/KittyIcon';

/** What the bar says for each route. The chat route shows the conversation's
    own title, and plain "Kitty" for a chat that has none yet. */
export function titleFor(view: HubView, chatTitle: string | null): string {
  switch (view) {
    case 'settings':
      return 'Settings';
    case 'wizard':
      return 'Setup';
    case 'sessions':
      return 'Saved chats';
    default:
      return chatTitle?.trim() || 'Kitty';
  }
}

/** The hub window's own title bar (desktop): the window is decorationless,
    the same as Bacon's, so this draws the drag area, the centred title and
    the minimise / maximise / close buttons itself. The left side is a slot:
    the chat route renders its actions into it (`ChatWorkspace`), so on the
    desktop there is no separate header row above the conversation.

    Close does what the native close button did: it closes this window and
    leaves Kitty running in the tray. Double-clicking the drag area maximises,
    as on a native bar. Android has no title bar. */
export function TitleBar({ actionsRef }: { actionsRef: (el: HTMLDivElement | null) => void }) {
  const view = useRouteStore((s) => s.view);
  const chatTitle = useChatStore((s) => s.title);

  if (isAndroid()) return null;
  const win = getCurrentWindow();
  const title = titleFor(view, chatTitle);

  return (
    <div className="titlebar">
      <div className="titlebar-drag" data-tauri-drag-region>
        <span className="titlebar-title" title={title}>
          <KittyIcon size={16} />
          <span className="titlebar-title-text">{title}</span>
        </span>
      </div>
      <div className="titlebar-left" ref={actionsRef} />
      <div className="titlebar-buttons">
        <button onClick={() => void win.minimize()} aria-label="Minimize" title="Minimize">
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0 5.5h10" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
        <button
          onClick={() => void win.toggleMaximize()}
          aria-label="Maximize or restore"
          title="Maximize"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none" aria-hidden="true">
            <rect x="0.5" y="0.5" width="9" height="9" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
        <button
          className="titlebar-close"
          onClick={() => void win.close()}
          aria-label="Close"
          title="Close"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
      </div>
    </div>
  );
}
