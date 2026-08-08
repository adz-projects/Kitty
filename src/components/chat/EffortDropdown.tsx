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
  // While a new session is being created, this still shows the *outgoing*
  // session's value (chatStore.ts's newSession() deliberately doesn't clear
  // it) — disable it rather than let a click try to set effort on a session
  // id that doesn't exist yet.
  const creatingSession = useChatStore((s) => s.creatingSession);

  if (!thinkingEffort) return null;

  // Guard against a `current_value` that isn't among the current `options`
  // (options can shrink after a model/profile change): React renders a select
  // with NO selected option when the value doesn't match any option, showing
  // a blank box and making the first click silently select whatever the
  // browser defaults to. Fall back to the first option as the display value
  // (the store keeps the true backend value untouched).
  const currentValue = thinkingEffort.options.some((o) => o.value === thinkingEffort.current_value)
    ? thinkingEffort.current_value
    : (thinkingEffort.options[0]?.value ?? '');

  return (
    <select
      className="effort-dropdown"
      title="Reasoning effort"
      aria-label="Reasoning effort"
      value={currentValue}
      disabled={creatingSession}
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
