// Feature detection for reasoning/thinking-capable models (Phase 10). This is a
// per-model hint that drives the *thinking indicator* before any content arrives;
// the reasoning panel itself is content-driven (shown whenever a model actually
// emits agent_thought_chunk), which is the ground truth. Patterns recorded in
// docs/VERSIONS.md — re-verify on model updates.

const REASONING_PATTERNS: RegExp[] = [
  /think/i, // lfm2.5-thinking, qwen3-thinking, *-thinking
  /reason/i,
  /deepseek-?r1/i,
  /\bqwq\b/i,
  /magistral/i,
  /\bo[1-4](-|\b)/i, // OpenAI o-series (o1/o3/o4)
  /\br1\b/i,
];

/** True if the model name suggests it streams a distinct reasoning trace. */
export function supportsReasoning(model: string | null | undefined): boolean {
  if (!model) return false;
  return REASONING_PATTERNS.some((re) => re.test(model));
}
