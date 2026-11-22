import { memo, useCallback, useState } from 'react';
import { ToolsIcon } from '@/components/icons/ToolsIcon';
import type { MemoryRecallSubsystem, MemoryRecallTrace } from '@/stores/chatStore';

function formatStatusLabel(status: string): string {
  switch (status) {
    case 'timed_out':
      return 'timed out';
    case 'seeded':
      return 'seeded in <think>';
    default:
      return status;
  }
}

function summaryTitle(recall: MemoryRecallTrace): string {
  const parts: string[] = [];
  const mem = recall.memorabilia;
  if (mem && (mem.status === 'injected' || (mem.item_count ?? 0) > 0)) {
    const n = mem.item_count ?? 0;
    parts.push(n > 0 ? `${n} fact${n === 1 ? '' : 's'}` : 'facts');
  }
  const pw = recall.pathway;
  if (pw && (pw.status === 'injected' || pw.status === 'seeded')) {
    parts.push(pw.status === 'seeded' ? 'profile (thought seed)' : 'profile');
  }
  if (recall.counterpoint) {
    parts.push('counterpoint');
  }
  if (recall.preflight) {
    parts.push('session recall');
  }
  if (parts.length > 0) {
    return `Memory recall · ${parts.join(', ')}`;
  }
  return 'Memory recall';
}

function statusBadge(recall: MemoryRecallTrace): string {
  const label = formatStatusLabel(recall.status);
  if (typeof recall.duration_ms === 'number' && recall.duration_ms >= 0) {
    return `${label} · ${recall.duration_ms}ms`;
  }
  return label;
}

function SubsystemBlock({ label, subsystem }: { label: string; subsystem: MemoryRecallSubsystem }) {
  if (subsystem.status === 'disabled') return null;
  const meta =
    typeof subsystem.item_count === 'number' && subsystem.item_count > 0
      ? `${label} (${subsystem.item_count} item${subsystem.item_count === 1 ? '' : 's'} · ${formatStatusLabel(subsystem.status)})`
      : `${label} (${formatStatusLabel(subsystem.status)})`;
  return (
    <>
      <div className="muted">{meta}</div>
      {subsystem.text ? (
        <pre>{subsystem.text}</pre>
      ) : (
        <pre>{`(${formatStatusLabel(subsystem.status)})`}</pre>
      )}
    </>
  );
}

/** Collapsible card inside `ThinkingBox` showing what turn-start memory recall
    injected (Memorabilia, Adaptive Pathway, Counterpoint, Preflight), styled
    identically to `ToolCallCard` so memory diagnostics live alongside tool
    calls without cluttering the chat bubble. */
export const MemoryRecallCard = memo(function MemoryRecallCard({
  recall,
}: {
  recall: MemoryRecallTrace;
}) {
  const [open, setOpen] = useState(false);
  const onToggle = useCallback((e: React.SyntheticEvent<HTMLDetailsElement>) => {
    setOpen(e.currentTarget.open);
  }, []);

  const hasStructured = Boolean(
    recall.memorabilia || recall.pathway || recall.counterpoint || recall.preflight
  );

  return (
    <details className="tool-card" onToggle={onToggle}>
      <summary>
        <span>
          <ToolsIcon /> {summaryTitle(recall)}
        </span>
        <span className="status-badge">{statusBadge(recall)}</span>
      </summary>
      {open && (
        <div className="tool-body">
          {recall.status === 'unchanged' && (
            <div className="muted">
              Matches the previous turn&apos;s block (already in context; skipped to preserve prompt
              cache).
            </div>
          )}
          {hasStructured ? (
            <>
              {recall.memorabilia && (
                <SubsystemBlock label="memorabilia" subsystem={recall.memorabilia} />
              )}
              {recall.pathway && (
                <SubsystemBlock label="adaptive pathway" subsystem={recall.pathway} />
              )}
              {recall.counterpoint && (
                <>
                  <div className="muted">counterpoint</div>
                  <pre>{recall.counterpoint}</pre>
                </>
              )}
              {recall.preflight && (
                <>
                  <div className="muted">session recall</div>
                  <pre>{recall.preflight}</pre>
                </>
              )}
            </>
          ) : recall.combined_block ? (
            <>
              <div className="muted">injected context</div>
              <pre>{recall.combined_block}</pre>
            </>
          ) : (
            <div className="muted">No memory items injected for this turn.</div>
          )}
        </div>
      )}
    </details>
  );
});
