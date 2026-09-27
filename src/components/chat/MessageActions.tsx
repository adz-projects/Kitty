import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { isLatestAnswer, useChatStore, type Message } from '@/stores/chatStore';
import { useMobileUiStore } from '@/stores/mobileUiStore';
import { usePopoverPosition } from '@/lib/usePopoverPosition';
import { isAndroid } from '@/lib/platform';
import { MessageInfo } from './MessageInfo';
import { BranchIcon } from '@/components/icons/BranchIcon';
import { RefreshIcon } from '@/components/icons/RefreshIcon';
import { CopyIcon } from '@/components/icons/CopyIcon';

/** Branch / Export / Regenerate / Copy / info for one message. Regenerate
 * appears on the latest answer only, and Branch/Regenerate wait while the chat
 * is replying. Export is desktop-only (see `exportSession`).
 *
 * Desktop: a hover-revealed row of text buttons under the message.
 * Android: nothing until the message is tapped (`MessageItem` toggles
 * `mobileUiStore.revealedMessageId`); then ⓘ and a ⋯ button that opens the
 * actions as a menu. Hover doesn't exist on a touchscreen, and four text
 * buttons plus ⓘ don't fit a phone-width row anyway. */
export function MessageActions({ message, index }: { message: Message; index: number }) {
  const actions = useMessageActionHandlers(message, index);
  return isAndroid() ? (
    <MobileMessageActions message={message} actions={actions} />
  ) : (
    <DesktopMessageActions message={message} actions={actions} />
  );
}

type Handlers = ReturnType<typeof useMessageActionHandlers>;

function useMessageActionHandlers(message: Message, index: number) {
  const branch = useChatStore((s) => s.branch);
  const regenerate = useChatStore((s) => s.regenerate);
  const exportSession = useChatStore((s) => s.exportSession);
  // Branching or regenerating mid-reply would cut the reply off (#41).
  const chatBusy = useChatStore((s) => s.busy);
  const canRegenerate = useChatStore((s) => isLatestAnswer(s.messages, index));

  // Branch/Regenerate/Export each fire a backend round-trip (fork/prompt/
  // export). Latch while one is running so a double-tap can't fork twice or
  // queue a duplicate prompt — the buttons visibly disable until it settles.
  const [busy, setBusy] = useState(false);
  const runOnce = (fn: () => Promise<unknown>) => () => {
    if (busy) return;
    setBusy(true);
    void Promise.resolve(fn()).finally(() => setBusy(false));
  };

  const [copied, setCopied] = useState(false);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Virtual-list rows unmount mid-write all the time (scrolling recycles
  // them) — the clipboard promise resolving after that must not setState.
  const mountedRef = useRef(true);
  useEffect(
    () => () => {
      mountedRef.current = false;
      if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
    },
    []
  );
  const copy = () => {
    void navigator.clipboard
      .writeText(message.text)
      .then(() => {
        if (!mountedRef.current) return;
        setCopied(true);
        if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
        copyTimerRef.current = setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => {
        /* clipboard may be unavailable */
      });
  };

  return {
    busy,
    chatBusy,
    canRegenerate,
    copied,
    copy,
    branch: runOnce(() => branch(index)),
    exportFromHere: runOnce(() => exportSession(index)),
    regenerate: runOnce(() => regenerate(index)),
  };
}

const BUSY_TITLE = 'Wait for the reply to finish';

function DesktopMessageActions({ message, actions }: { message: Message; actions: Handlers }) {
  const { busy, chatBusy, copied } = actions;
  return (
    <div className="msg-actions">
      <button
        title={chatBusy ? BUSY_TITLE : 'Branch a new session from here'}
        disabled={busy || chatBusy}
        onClick={actions.branch}
      >
        Branch
      </button>
      <button
        title="Export the conversation up to here as ChatML"
        disabled={busy}
        onClick={actions.exportFromHere}
      >
        Export from here
      </button>
      {message.role === 'assistant' && actions.canRegenerate && (
        <button
          title={chatBusy ? BUSY_TITLE : 'Regenerate this response'}
          disabled={busy || chatBusy}
          onClick={actions.regenerate}
        >
          Regenerate
        </button>
      )}
      <button
        title={message.role === 'assistant' ? 'Copy as Markdown' : 'Copy'}
        onClick={actions.copy}
      >
        {copied ? 'Copied' : 'Copy'}
      </button>
      {message.role === 'assistant' && <MessageInfo message={message} />}
    </div>
  );
}

function MobileMessageActions({ message, actions }: { message: Message; actions: Handlers }) {
  const revealed = useMobileUiStore((s) => s.revealedMessageId === message.id);
  const hide = useMobileUiStore((s) => s.hideMessageActions);
  const [menuOpen, setMenuOpen] = useState(false);
  // Outside taps are handled by the backdrop below, not the hook: the hook's
  // document-level `pointerdown` would close the menu and then let the same
  // tap land on whatever is underneath — toggling another message's actions,
  // or following a link.
  const { triggerRef, popoverRef, style } = usePopoverPosition(menuOpen, () => {});

  if (!revealed) return null;

  const assistant = message.role === 'assistant';
  const pick = (fn: () => void, keepRow = false) => {
    setMenuOpen(false);
    if (!keepRow) hide();
    fn();
  };

  return (
    <div className="msg-actions msg-actions-mobile">
      {assistant && <MessageInfo message={message} />}
      <button
        ref={triggerRef as React.Ref<HTMLButtonElement>}
        className="msg-more"
        aria-label="More actions"
        aria-haspopup="menu"
        aria-expanded={menuOpen}
        onClick={() => setMenuOpen((o) => !o)}
      >
        ⋯
      </button>
      {actions.copied && <span className="muted msg-copied">Copied</span>}
      {menuOpen &&
        // Portaled for the same reason as `MessageInfo`'s popover: virtual
        // rows are transformed, which would re-anchor `position: fixed`.
        createPortal(
          <div className="msg-menu-backdrop" onClick={() => setMenuOpen(false)}>
            <div
              ref={popoverRef}
              className="mode-popover msg-actions-menu"
              role="menu"
              style={style}
              onClick={(e) => e.stopPropagation()}
            >
              <button
                role="menuitem"
                disabled={actions.busy || actions.chatBusy}
                onClick={() => pick(actions.branch)}
              >
                <BranchIcon /> Branch
              </button>
              {assistant && actions.canRegenerate && (
                <button
                  role="menuitem"
                  disabled={actions.busy || actions.chatBusy}
                  onClick={() => pick(actions.regenerate)}
                >
                  <RefreshIcon /> Regenerate
                </button>
              )}
              {/* Keeps the row showing, so its "Copied" confirmation is seen. */}
              <button role="menuitem" onClick={() => pick(actions.copy, true)}>
                <CopyIcon /> Copy
              </button>
            </div>
          </div>,
          document.body
        )}
    </div>
  );
}
