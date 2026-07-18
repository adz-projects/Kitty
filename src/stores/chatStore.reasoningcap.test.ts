import { describe, it, expect } from 'vitest';
import { exceedsReasoningCap } from './chatStore';

/** Backs the recipe reasoning hard cap (`Recipe.max_reasoning_tokens`,
    default 2048): unlike general chat's loop-detection *suggestion*, a
    recipe-invoked turn is meant to be auto-cancelled once its reasoning
    exceeds the configured cap. Goose exposes no numeric reasoning-token
    count via ACP (only effort levels), so this is approximated via
    character count (~4 chars/token for English text) — these tests pin that
    approximation's exact boundary behavior. */

const CHARS_PER_TOKEN = 4;

describe('exceedsReasoningCap', () => {
  it('is false when comfortably under the cap', () => {
    expect(exceedsReasoningCap(100, 2048)).toBe(false);
  });

  it('is false exactly at the cap boundary', () => {
    const lengthAtCap = 2048 * CHARS_PER_TOKEN;
    expect(exceedsReasoningCap(lengthAtCap, 2048)).toBe(false);
  });

  it('is true just past the cap boundary', () => {
    const lengthJustOver = 2048 * CHARS_PER_TOKEN + 1;
    expect(exceedsReasoningCap(lengthJustOver, 2048)).toBe(true);
  });

  it('is false for zero-length reasoning', () => {
    expect(exceedsReasoningCap(0, 2048)).toBe(false);
  });

  it('respects a custom, smaller cap', () => {
    expect(exceedsReasoningCap(500, 100)).toBe(true);
    expect(exceedsReasoningCap(390, 100)).toBe(false); // 390/4 = 97.5 -> ceil 98
  });
});
