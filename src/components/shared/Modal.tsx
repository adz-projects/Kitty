import type { ReactNode } from 'react';
import { Dialog } from './Dialog';

/** Shared modal chrome — now a plain `Dialog` (focus trap, Escape and Back
    through the shared layer stacks). Kept as a name for its existing callers. */
export function Modal({
  title,
  children,
  onClose,
}: {
  title: string;
  children: ReactNode;
  onClose: () => void;
}) {
  return (
    <Dialog title={title} onClose={onClose}>
      {children}
    </Dialog>
  );
}
