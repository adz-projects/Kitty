/** Real-terms progress while waiting for a response (Round-5 Batch 6) — replaces
    the bare animated ellipsis. The stage is derived from existing signals (see
    `useProgressStage`): `connecting` (sent, nothing back yet) → `thinking`
    (reasoning streaming) → `formulating` (reasoning done / composing the answer).
    The label text itself pulses (opacity), so it still reads as live activity
    the way the dots did. Settles when the answer text starts rendering. */
export type ProgressStage = 'connecting' | 'thinking' | 'formulating';

export const PROGRESS_LABELS: Record<ProgressStage, string> = {
  connecting: 'Connecting to the server',
  thinking: 'Provider is thinking',
  formulating: 'Formulating a response',
};

export function ThinkingIndicator({ stage }: { stage: ProgressStage }) {
  return (
    <div className="msg msg-assistant">
      <span className="progress-indicator" role="status" aria-label={PROGRESS_LABELS[stage]}>
        <span className="progress-dot" aria-hidden="true" />
        <span className="progress-pulse">{PROGRESS_LABELS[stage]}…</span>
      </span>
    </div>
  );
}
