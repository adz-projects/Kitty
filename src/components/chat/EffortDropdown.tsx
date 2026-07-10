import { useChatStore } from '@/stores/chatStore';

/** Reasoning-effort control for the active session (Round-7) — live,
    per-session, no goosed restart (unlike provider/temperature/model, which
    are spawn-time settings). Renders nothing when the active model doesn't
    support effort control at all (see chatStore.ts's `thinkingEffort`, `null`
    in that case) — there's nothing useful to offer for e.g. a plain Ollama
    chat model with no extended-thinking mode. */
export function EffortDropdown() {
  const thinkingEffort = useChatStore((s) => s.thinkingEffort);
  const setThinkingEffort = useChatStore((s) => s.setThinkingEffort);

  if (!thinkingEffort) return null;

  return (
    <select
      className="effort-dropdown"
      title="Reasoning effort"
      aria-label="Reasoning effort"
      value={thinkingEffort.current_value}
      onChange={(e) => void setThinkingEffort(e.target.value)}
    >
      {thinkingEffort.options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.name}
        </option>
      ))}
    </select>
  );
}
