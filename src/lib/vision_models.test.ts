import { describe, it, expect } from 'vitest';
import { supportsImages } from './vision_models';

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
