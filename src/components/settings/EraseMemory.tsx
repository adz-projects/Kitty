import { useState } from 'react';
import { confirmDialog } from '@/components/shared/ConfirmDialog';

/** "Erase all" for one memory engine: behind a typed confirmation, because
    nothing brings it back. */
export function EraseMemory({
  what,
  phrase,
  erase,
  onErased,
}: {
  /** "beliefs", "facts" */
  what: string;
  /** What the user types to confirm. */
  phrase: string;
  erase: () => Promise<void>;
  onErased?: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);

  const run = async () => {
    const ok = await confirmDialog({
      title: `Erase all ${what}?`,
      message: `Everything Kitty has learned here is deleted for good. Chats are not affected.`,
      confirmLabel: `Erase all ${what}`,
      danger: true,
      typed: phrase,
    });
    if (!ok) return;
    setBusy(true);
    setResult(null);
    try {
      await erase();
      setResult(`All ${what} erased.`);
      onErased?.();
    } catch (e) {
      setResult(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mem-danger-row">
      <div>
        <div>Start over</div>
        <small className="muted">Permanently clear stored {what}. Chats are not affected.</small>
        {result && (
          <div>
            <small className="muted">{result}</small>
          </div>
        )}
      </div>
      <button className="danger" disabled={busy} onClick={() => void run()}>
        {busy ? 'Erasing…' : `Erase all ${what}…`}
      </button>
    </div>
  );
}
