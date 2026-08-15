// Feature detection for vision-capable (image-accepting) models. The backend
// exposes no reliable per-model capability signal for this, so, same as
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
  // Qwen3.6 ships multimodal by default (confirmed via a live server's
  // `/api/tags` reporting `capabilities: ["completion", "multimodal"]` for
  // "Qwen3.6-27B-..."). Unlike earlier Qwen3.x releases (`qwen3:4b` etc. are
  // still text-only, hence not matched by the generic `qwen.*-?vl` pattern
  // above), it dropped the separate "-VL" suffix naming convention those
  // patterns rely on, so it needs its own explicit entry.
  /qwen3\.6/i,
];

const NON_VISION_OVERRIDES: RegExp[] = [/o1-mini/i, /o3-mini/i, /o4-mini/i];

/** True if the model name suggests it accepts image content blocks. */
export function supportsImages(model: string | null | undefined): boolean {
  if (!model) return false;
  if (NON_VISION_OVERRIDES.some((re) => re.test(model))) return false;
  return VISION_PATTERNS.some((re) => re.test(model));
}

/** Detection OR the provider profile's manual `supports_vision` override.
 *
 * Use this, not `supportsImages`, anywhere a decision is made about what the
 * *user* may attach. The patterns above cannot know a self-hosted or
 * unconventionally-named vision model, and defaulting those to "no" (correct
 * as a default) would otherwise leave no way to say so. The override only
 * widens — it can enable image affordances for a model the patterns miss, and
 * never disables them for one they recognize. */
export function modelAcceptsImages(
  model: string | null | undefined,
  providerOverride: boolean
): boolean {
  return providerOverride || supportsImages(model);
}
