import { ProviderBadge } from './ProviderBadge';
import { EffortDropdown } from './EffortDropdown';
import { ChatHeaderMenu } from './ChatHeaderMenu';

/** The per-session controls that belong to the conversation rather than to the
    window: the provider (model) picker, reasoning effort, and the overflow
    menu.
 *
 * Extracted from `ChatView` because Android renders them somewhere else. On
 * desktop they sit in the chat's own header, under the window header. A phone
 * has no vertical room to spend on two header rows for one conversation, so
 * `ChatWorkspace` hoists this into the window header beside "Show artifacts"
 * and "New chat" and `ChatView` drops its header entirely. Same components,
 * same state, different parent — nothing here knows which. */
export function ChatHeaderControls() {
  return (
    <div className="chat-header-controls">
      <ProviderBadge />
      <EffortDropdown />
      {/* The overflow menu: incognito on both platforms, and on Android the
          chat's allowed folders too (#79). */}
      <ChatHeaderMenu />
    </div>
  );
}
