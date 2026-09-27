import { useEffect } from 'react';
import { Dialog } from '@/components/shared/Dialog';
import { offScreenApprovals, useApprovalStore } from '@/stores/approvalStore';
import { useSessionStore } from '@/stores/sessionStore';
import { useChatStore } from '@/stores/chatStore';
import { ApprovalPrompt } from './ApprovalPrompt';

/** The interrupting dialog for an approval from a chat that is not on screen
    in this window (decision #7): another chat mid-turn, or a scheduled task.
    Blocking, because the turn is paused on it — the only ways out are the
    answers themselves, or opening that chat to decide there.

    `onScreenSessionId` is the chat visible in this window, whose approvals
    appear inline instead; null when no chat is visible. One approval at a
    time, oldest first; the next appears as soon as it is answered. */
export function ApprovalModal({
  onScreenSessionId,
  canOpenChat = true,
}: {
  onScreenSessionId: string | null;
  /** Whether "Open chat" may load the chat into this window. */
  canOpenChat?: boolean;
}) {
  const init = useApprovalStore((s) => s.init);
  const pending = useApprovalStore((s) => s.pending);
  const error = useApprovalStore((s) => s.error);
  const sessions = useSessionStore((s) => s.sessions);

  useEffect(() => {
    void init();
  }, [init]);

  const approval = offScreenApprovals(pending, onScreenSessionId)[0];
  if (!approval) return null;

  const chat = sessions.find((s) => s.sessionId === approval.session_id);
  const title = approval.scheduled
    ? 'A scheduled task needs your approval'
    : chat?.title
      ? `"${chat.title}" needs your approval`
      : 'Another chat needs your approval';

  const openChat = () => {
    if (!chat) return;
    void useChatStore
      .getState()
      .loadSession(chat.sessionId, chat.cwd, chat.title, chat.providerId, chat.modelId);
  };

  return (
    <Dialog title={title} onClose={() => {}} blocking alert className="approval-modal">
      <ApprovalPrompt approval={approval} inline={false} />
      {error && <p className="error">{error}</p>}
      {canOpenChat && chat && (
        <div className="modal-actions">
          <button className="link" onClick={openChat}>
            Open that chat instead
          </button>
        </div>
      )}
    </Dialog>
  );
}
