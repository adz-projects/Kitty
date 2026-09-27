import { useState } from 'react';
import { create } from 'zustand';
import { Dialog } from './Dialog';

export interface ConfirmOptions {
  title: string;
  message: string;
  confirmLabel?: string;
  /** Styles the confirm button as destructive. */
  danger?: boolean;
  /** The user must type this exact text before confirming — for the
      irreversible ("Erase all beliefs") rather than the merely final. */
  typed?: string;
}

interface Pending extends ConfirmOptions {
  resolve: (ok: boolean) => void;
}

const useConfirmStore = create<{ pending: Pending | null }>(() => ({ pending: null }));

/** In-app replacement for `window.confirm`: resolves `true` on confirm and
    `false` on cancel, Escape or Back. Needs a `<ConfirmHost />` mounted in
    the window. A second request while one is open cancels the first. */
export function confirmDialog(options: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => {
    useConfirmStore.getState().pending?.resolve(false);
    useConfirmStore.setState({ pending: { ...options, resolve } });
  });
}

/** Whether `input` satisfies a typed confirmation. Pure, for tests. */
export function typedMatches(expected: string | undefined, input: string): boolean {
  return !expected || input.trim() === expected;
}

export function ConfirmHost() {
  const pending = useConfirmStore((s) => s.pending);
  if (!pending) return null;
  return <ConfirmDialogView key={pending.title + pending.message} pending={pending} />;
}

function ConfirmDialogView({ pending }: { pending: Pending }) {
  const [typed, setTyped] = useState('');
  const finish = (ok: boolean) => {
    useConfirmStore.setState({ pending: null });
    pending.resolve(ok);
  };
  const allowed = typedMatches(pending.typed, typed);
  return (
    <Dialog title={pending.title} onClose={() => finish(false)} alert>
      <p style={{ margin: 0 }}>{pending.message}</p>
      {pending.typed && (
        <label className="field">
          <span>
            Type <strong>{pending.typed}</strong> to confirm
          </span>
          <input
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && allowed) finish(true);
            }}
            autoComplete="off"
            spellCheck={false}
          />
        </label>
      )}
      <div className="modal-actions">
        <button onClick={() => finish(false)}>Cancel</button>
        <button
          className={pending.danger ? 'danger' : 'primary'}
          disabled={!allowed}
          onClick={() => finish(true)}
        >
          {pending.confirmLabel ?? 'OK'}
        </button>
      </div>
    </Dialog>
  );
}
