// Built-in system-prompt defaults (Round-6 Feature 2), used when a provider
// profile has no explicit `system_prompt` override. Prepended client-side to
// the first outgoing message of a new session (chatStore.ts's `send()`) —
// Goose's ACP `session/new` has no system-prompt field it honors
// (docs/acp-protocol.md). Complementary to, not duplicative of, the
// GOOSE_MOIM_MESSAGE_TEXT save-path nudge in providers.rs's `goosed_env()`,
// which stays as-is and is injected every turn regardless of this mechanism.

export const AGENTIC_SYSTEM_PROMPT = `You are a capable, direct agentic assistant. You have
filesystem and shell tools scoped to this conversation's own working directory. Use tools
proactively rather than describing what you would do — take the action. When you create or
save a file, use a relative path inside the working directory rather than an absolute path
elsewhere. Be direct about assumptions and uncertainty rather than glossing over them, and
prefer verifiable action (running a command, reading a file, writing output) over speculation.`;

/** The built-in default. Overridden per-provider by
    `ProviderProfile.system_prompt` when set.
    There used to be a second, chat-only prompt selected by the per-session
    chat/agentic toggle. That toggle is gone, and with it the reason to tell
    the model it might not have tools: whether it does is now the provider's
    property, not a mode the user set, and a provider that can't call tools
    simply never gets any in its request. */
export function defaultSystemPrompt(): string {
  return AGENTIC_SYSTEM_PROMPT;
}
