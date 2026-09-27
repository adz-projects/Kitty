import { useEffect, useId, useRef, type ReactNode } from 'react';
import { useBackDismiss } from '@/hooks/useBackDismiss';
import { useEscapeLayer } from '@/hooks/useEscapeLayer';

const FOCUSABLE =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/** Modal dialog chrome with the behaviour a dialog owes its users: focus moves
    in and stays in (Tab cycles), Escape and Android Back close it,
    and focus returns to where it was on close.

    `blocking` removes every way out except the dialog's own buttons — for a
    decision that must be made (a tool approval), not merely acknowledged.
    A backdrop click closes only with `closeOnBackdrop`: a stray click must
    not throw away a half-filled form. */
export function Dialog({
  title,
  children,
  onClose,
  blocking = false,
  alert = false,
  closeOnBackdrop = false,
  className,
}: {
  title: string;
  children: ReactNode;
  onClose: () => void;
  blocking?: boolean;
  /** `alertdialog`: interrupts the user and needs a response. */
  alert?: boolean;
  closeOnBackdrop?: boolean;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useEscapeLayer(!blocking, onClose);
  useBackDismiss(!blocking, onClose);

  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const first = ref.current?.querySelector<HTMLElement>(FOCUSABLE);
    (first ?? ref.current)?.focus();
    return () => previous?.focus?.();
  }, []);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (blocking && e.key === 'Escape') {
      // Swallowed so the overlay doesn't hide itself out from under a
      // decision it is waiting on.
      e.stopPropagation();
      return;
    }
    if (e.key !== 'Tab' || !ref.current) return;
    const items = Array.from(ref.current.querySelectorAll<HTMLElement>(FOCUSABLE));
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  };

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (closeOnBackdrop && !blocking && e.target === e.currentTarget) onClose();
      }}
    >
      <div
        ref={ref}
        className={className ? `modal ${className}` : 'modal'}
        role={alert ? 'alertdialog' : 'dialog'}
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        onKeyDown={onKeyDown}
      >
        <h2 id={titleId}>{title}</h2>
        {children}
      </div>
    </div>
  );
}
