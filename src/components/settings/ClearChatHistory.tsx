import { useState } from 'react';
import { ipc } from '@/lib/ipc';
import { Modal } from '@/components/shared/Modal';

/** The "Clear all chat history" danger zone, extracted so it can live in two
    places: Settings → General on desktop, and Settings → Advanced on Android
    (where General is removed entirely). Same behaviour in both — a confirm
    modal that reports the session count and calls `ipc.clearAllSessions`. */
export function ClearChatHistory() {
  const [confirmClear, setConfirmClear] = useState(false);
  const [sessionCount, setSessionCount] = useState<number | null>(null);
  const [clearing, setClearing] = useState(false);
  const [clearError, setClearError] = useState<string | null>(null);

  const openConfirmClear = async () => {
    setClearError(null);
    setConfirmClear(true);
    try {
      setSessionCount((await ipc.listSessions()).length);
    } catch {
      setSessionCount(null);
    }
  };

  return (
    <>
      <div className="field">
        <span>Danger zone</span>
        <button onClick={() => void openConfirmClear()}>Clear all chat history</button>
        <small className="muted">
          Permanently deletes every conversation and its working-directory files.
        </small>
      </div>

      {confirmClear && (
        <Modal title="Clear all chat history?">
          <p>
            This permanently deletes {sessionCount ?? 'all'} conversation(s) and their
            working-directory files. This cannot be undone.
          </p>
          {clearError && <div className="chat-error">{clearError}</div>}
          <div className="row">
            <button
              className="primary"
              disabled={clearing}
              onClick={async () => {
                setClearing(true);
                setClearError(null);
                try {
                  await ipc.clearAllSessions();
                  setConfirmClear(false);
                } catch (e) {
                  setClearError(String(e));
                } finally {
                  setClearing(false);
                }
              }}
            >
              {clearing ? 'Deleting…' : 'Yes, delete everything'}
            </button>
            <button onClick={() => setConfirmClear(false)} disabled={clearing}>
              Cancel
            </button>
          </div>
        </Modal>
      )}
    </>
  );
}
