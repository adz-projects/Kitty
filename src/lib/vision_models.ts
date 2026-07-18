// Feature detection for vision-capable (image-accepting) models. ACP exposes no
// per-model capability signal for this — `initialize`'s `agentCapabilities.
// promptCapabilities.image` (docs/acp-protocol.md) is agent-level, reporting
// whether Goose itself can carry an image content block at all, not whether
// the specific active model can actually see it. So, same as
// reasoning_models.ts, this is a name-pattern heuristic — re-verify on model
// updates.
//
// Deliberately conservative: an unrecognized model defaults to "no vision"
// rather than "assume vision," since the failure mode of wrongly allowing an
// attach is a wasted/failed turn, while the failure mode of wrongly blocking
// one is just having to skip the attachment. Patterns cover the vision-capable
// model families in common use; text-only variants within an otherwise
// vision-capable family (e.g. `-mini` reasoning models that drop vision) are
// carved out via NON_VISION_OVERRIDES.

const VISION_PATTERNS: RegExp[] = [
  /gpt-4o/i,
  /gpt-4\.1/i,
  /gpt-4-turbo/i,
  /gpt-4-vision/i,
  /gpt-5/i,
  /\bo1\b/i,
  /\bo3\b/i,
  /\bo4\b/i,
  /claude-3/i,
  /claude-(sonnet|opus|haiku)-4/i,
  /gemini/i,
  /pixtral/i,
  /llama-3\.2.*vision/i,
  /llama-4/i,
  /qwen.*-?vl/i,
  /qwen2\.5-vl/i,
  /grok-(2-)?vision/i,
  /grok-4/i,
  /llava/i,
  /bakllava/i,
  /moondream/i,
  /minicpm-v/i,
  /internvl/i,
  /phi-3\.5-vision/i,
  /phi-4.*vision/i,
];

const NON_VISION_OVERRIDES: RegExp[] = [/o1-mini/i, /o3-mini/i];

/** True if the model name suggests it accepts image content blocks. */
export function supportsImages(model: string | null | undefined): boolean {
  if (!model) return false;
  if (NON_VISION_OVERRIDES.some((re) => re.test(model))) return false;
  return VISION_PATTERNS.some((re) => re.test(model));
}
