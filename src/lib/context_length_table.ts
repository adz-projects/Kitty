// Hardcoded context-window fallback tables for provider types with no live
// lookup API (Round-6 Feature 1): Ollama uses /api/show and OpenRouter uses
// /api/v1/models instead (see src-tauri/src/ollama, src-tauri/src/openrouter).
// Anthropic has no public model-listing API; a generic OpenAI-compatible
// backend's /v1/models response isn't guaranteed to include context length
// either (OpenAI's own doesn't). Re-verify against each vendor's docs
// periodically — recorded in docs/VERSIONS.md.

export interface ContextLengthEntry {
  match: RegExp;
  context_length: number;
}

// Current Claude models are all 200K standard context. The 1M-beta tier
// requires a beta header Kitty doesn't send, so 200K is what's actually
// honored today regardless of which Claude model is selected.
export const ANTHROPIC_CONTEXT_TABLE: ContextLengthEntry[] = [
  { match: /claude/i, context_length: 200_000 },
];

export const CUSTOM_OPENAI_CONTEXT_TABLE: ContextLengthEntry[] = [
  { match: /gpt-4o|gpt-4\.1/i, context_length: 128_000 },
  { match: /gpt-4-turbo/i, context_length: 128_000 },
  { match: /gpt-3\.5/i, context_length: 16_385 },
];

/** First regex match wins; `null` when nothing matches (manual override only). */
export function lookupContextLength(table: ContextLengthEntry[], model: string): number | null {
  return table.find((e) => e.match.test(model))?.context_length ?? null;
}
