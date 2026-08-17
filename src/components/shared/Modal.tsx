import { useEffect, type ReactNode } from 'react';

/** Shared modal chrome (`.modal-backdrop`/`.modal`) — extracted from
    `Providers.tsx`'s original local, unexported copy once a second consumer
    (`SchismResolutionModal.tsx`, Round-C) justified sharing it.
    `onClose` is called on Escape; `e.stopPropagation()` mirrors the pattern
    in `Composer.tsx`'s autocomplete Escape handler so this doesn't also
    trigger the overlay window's own Escape-to-hide handler. */
export function Modal({
  title,
  children,
  onClose,
}: {
  title: string;
  children: ReactNode;
  onClose: () => void;
}) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [onClose]);

  return (
    <div className="modal-backdrop">
      <div className="modal">
        <h2>{title}</h2>
        {children}
      </div>
    </div>
  );
}
