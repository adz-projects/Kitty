import { describe, it, expect } from 'vitest';
import { supportsImages, modelAcceptsImages } from './vision_models';

describe('supportsImages', () => {
  it('recognizes common vision-capable model families', () => {
    for (const m of [
      'gpt-4o',
      'claude-sonnet-4-20260101',
      'gemini-2.0-flash',
      'llava:13b',
      'qwen2-vl:7b',
    ]) {
      expect(supportsImages(m)).toBe(true);
    }
  });

  it('rejects text-only models', () => {
    for (const m of ['llama3.1:8b', 'mistral:7b', 'qwen3:4b', 'deepseek-r1:14b']) {
      expect(supportsImages(m)).toBe(false);
    }
  });

  it('recognizes Qwen3.6 as multimodal despite dropping the -VL suffix convention', () => {
    // Confirmed live against a custom llama.cpp server's /api/tags, which
    // reports capabilities: ["completion", "multimodal"] for this release.
    for (const m of ['Qwen3.6-27b', '/models/Qwen3.6-27B-UD-Q5_K_XL.gguf', 'qwen3.6:27b']) {
      expect(supportsImages(m)).toBe(true);
    }
  });

  it('carves out non-vision -mini variants of an otherwise-vision family', () => {
    expect(supportsImages('o1-mini')).toBe(false);
    expect(supportsImages('o3-mini')).toBe(false);
    expect(supportsImages('o1')).toBe(true);
  });

  it('defaults to false for null/undefined/empty', () => {
    expect(supportsImages(null)).toBe(false);
    expect(supportsImages(undefined)).toBe(false);
    expect(supportsImages('')).toBe(false);
  });
});

describe('modelAcceptsImages', () => {
  it("lets the provider override rescue a vision model the patterns don't know", () => {
    // The case the override exists for: a self-hosted or renamed vision model.
    expect(supportsImages('my-finetune-v2')).toBe(false);
    expect(modelAcceptsImages('my-finetune-v2', true)).toBe(true);
  });

  it('only ever widens — an off override never hides a recognized vision model', () => {
    expect(modelAcceptsImages('gpt-4o', false)).toBe(true);
    expect(modelAcceptsImages('gpt-4o', true)).toBe(true);
  });

  it('stays false for a text-only model with no override', () => {
    expect(modelAcceptsImages('llama3.2:3b', false)).toBe(false);
  });

  it('honors the override even with no model selected', () => {
    expect(modelAcceptsImages(null, true)).toBe(true);
    expect(modelAcceptsImages(null, false)).toBe(false);
  });
});

describe('modelAcceptsImages — detected capability', () => {
  /** The failure the whole detection pass exists to end: a model whose name
      matches no pattern (self-hosted, renamed) had its image affordances
      hidden with no recourse but a manual tick. A provider that reports its
      own capabilities settles it. */
  it('lets a detected yes enable images for a model the patterns miss', () => {
    expect(supportsImages('internal-vlm-v3')).toBe(false);
    expect(modelAcceptsImages('internal-vlm-v3', false, true)).toBe(true);
  });

  /** The mirror image, and the reason detection is authoritative rather than
      widening-only: a text-only model with a vision-ish name used to offer an
      attach button that could only ever produce a failed turn. */
  it('lets a detected no disable images for a model the patterns match', () => {
    expect(supportsImages('gemini-text-only-preview')).toBe(true);
    expect(modelAcceptsImages('gemini-text-only-preview', false, false)).toBe(false);
  });

  it('falls back to name patterns when nothing was detected', () => {
    for (const detected of [null, undefined]) {
      expect(modelAcceptsImages('gpt-4o', false, detected)).toBe(true);
      expect(modelAcceptsImages('mistral:7b', false, detected)).toBe(false);
    }
  });

  /** The manual override still outranks everything — including a detected
      no. The user ticked a box about their own endpoint. */
  it('keeps the manual override on top', () => {
    expect(modelAcceptsImages('anything', true, false)).toBe(true);
    expect(modelAcceptsImages(null, true, null)).toBe(true);
  });
});
