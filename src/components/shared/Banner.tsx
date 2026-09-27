import type { ReactNode } from 'react';

/** One-line status strip with a coloured dot and optional actions — the
    pattern `ChatView` repeated inline for every notice. */
export function Banner({
  tone,
  children,
  actions,
}: {
  tone: 'ok' | 'warn' | 'bad';
  children: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div className="conflict-banner" role="status">
      <span className={`status-dot ${tone}`} />
      <span style={{ flex: 1 }}>{children}</span>
      {actions}
    </div>
  );
}
