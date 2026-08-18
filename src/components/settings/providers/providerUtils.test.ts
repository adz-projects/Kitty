import { describe, it, expect } from 'vitest';
import { canSaveProvider, usesModelPicker } from './providerUtils';

describe('usesModelPicker', () => {
  it('is false for local/custom_openai/ollama — they keep the free-text form', () => {
    expect(usesModelPicker('local')).toBe(false);
    expect(usesModelPicker('custom_openai')).toBe(false);
    expect(usesModelPicker('ollama')).toBe(false);
  });

  it('is true for every hosted type the new flow covers', () => {
    expect(usesModelPicker('openrouter')).toBe(true);
    expect(usesModelPicker('anthropic')).toBe(true);
    expect(usesModelPicker('openai')).toBe(true);
    expect(usesModelPicker('fireworks')).toBe(true);
    expect(usesModelPicker('qwen_cloud')).toBe(true);
    expect(usesModelPicker('deepinfra')).toBe(true);
  });
});

describe('canSaveProvider', () => {
  it('legacy-form types are always savable regardless of the models array', () => {
    expect(canSaveProvider('local', [])).toBe(true);
    expect(canSaveProvider('custom_openai', [])).toBe(true);
    expect(canSaveProvider('custom_openai', ['a', 'b', 'c'])).toBe(true);
  });

  it('new-flow types require exactly one non-empty model', () => {
    expect(canSaveProvider('openrouter', [])).toBe(false);
    expect(canSaveProvider('openrouter', ['anthropic/claude-sonnet-5'])).toBe(true);
  });

  it('new-flow types reject more than one model', () => {
    expect(canSaveProvider('anthropic', ['model-a', 'model-b'])).toBe(false);
  });

  it('new-flow types reject a single blank/whitespace-only model', () => {
    expect(canSaveProvider('openai', [''])).toBe(false);
    expect(canSaveProvider('openai', ['   '])).toBe(false);
  });
});
