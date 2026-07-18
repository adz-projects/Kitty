import type { ReactNode } from 'react';

/** Shared modal chrome (`.modal-backdrop`/`.modal`) — extracted from
    `Providers.tsx`'s original local, unexported copy once a second consumer
    (`SchismResolutionModal.tsx`, Round-C) justified sharing it. */
export function Modal({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="modal-backdrop">
      <div className="modal">
        <h2>{title}</h2>
        {children}
      </div>
    </div>
  );
}
