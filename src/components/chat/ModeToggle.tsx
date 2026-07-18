import { isChatMode, useChatStore } from '@/stores/chatStore';
import { ChatBubbleIcon } from '@/components/icons/ChatBubbleIcon';
import { ToolsIcon } from '@/components/icons/ToolsIcon';

/** Instant per-session chat/agentic toggle (Round-4) — flips the session's
    effective mode with no provider switch and no goosed respawn (the previous
    only way to change mode). See `chatStore.ts`'s `setModeOverride` for the
    tool-safety handling this drives on flip. */
export function ModeToggle() {
  const chatMode = useChatStore(isChatMode);
  const setModeOverride = useChatStore((s) => s.setModeOverride);

  return (
    <div className="mode-toggle" role="group" aria-label="Chat or agentic mode">
      <button
        className={chatMode ? 'active' : ''}
        title="Chat mode — no tool calls, reading-friendly layout"
        onClick={() => void setModeOverride('chat')}
      >
        <ChatBubbleIcon /> <span className="mode-toggle-label">Chat</span>
      </button>
      <button
        className={!chatMode ? 'active' : ''}
        title="Agentic mode — tools, file context, working directory"
        onClick={() => void setModeOverride('agentic')}
      >
        <ToolsIcon /> <span className="mode-toggle-label">Agent</span>
      </button>
    </div>
  );
}
