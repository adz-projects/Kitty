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
    expect(usesModelPicker('deepinfra')).toBe(true);
  });
});

describe('canSaveProvider', () => {
  it('requires exactly one non-empty model', () => {
    expect(canSaveProvider([])).toBe(false);
    expect(canSaveProvider(['anthropic/claude-sonnet-5'])).toBe(true);
  });

  it('rejects more than one model (duplicate the card instead)', () => {
    expect(canSaveProvider(['model-a', 'model-b'])).toBe(false);
  });

  it('rejects a single blank/whitespace-only model', () => {
    expect(canSaveProvider([''])).toBe(false);
    expect(canSaveProvider(['   '])).toBe(false);
  });
});
