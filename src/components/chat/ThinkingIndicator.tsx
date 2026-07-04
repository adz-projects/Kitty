/** Shown while a response is streaming but no visible text has arrived yet
    (Phase 10). Reasoning-capable models get a distinct "Thinking…" animation so
    it's clear the model is reasoning, not just slow; others get plain typing dots. */
export function ThinkingIndicator({ reasoning }: { reasoning: boolean }) {
  if (reasoning) {
    return (
      <span className="thinking-indicator" aria-label="Thinking">
        <span className="thinking-glyph">🧠</span> Thinking
        <span className="thinking-dots">
          <i />
          <i />
          <i />
        </span>
      </span>
    );
  }
  return (
    <span className="typing" aria-label="Assistant is typing">
      <span className="thinking-dots">
        <i />
        <i />
        <i />
      </span>
    </span>
  );
}
