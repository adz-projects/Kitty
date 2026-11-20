import { describe, it, expect } from 'vitest';
import { canSaveProvider, hasFixedEndpoint, initialChecks } from './providerUtils';

describe('hasFixedEndpoint', () => {
  it('is false for local/custom_openai/ollama — they need an editable URL', () => {
    expect(hasFixedEndpoint('local')).toBe(false);
    expect(hasFixedEndpoint('custom_openai')).toBe(false);
    expect(hasFixedEndpoint('ollama')).toBe(false);
  });

  it('is true for every hosted service', () => {
    for (const t of ['openrouter', 'anthropic', 'openai', 'fireworks', 'deepinfra'] as const) {
      expect(hasFixedEndpoint(t)).toBe(true);
    }
  });
});

describe('canSaveProvider', () => {
  it('needs at least one checked, listed model', () => {
    expect(canSaveProvider({ models: [], enabled_models: [] })).toBe(false);
    expect(canSaveProvider({ models: ['a', 'b'], enabled_models: [] })).toBe(false);
    expect(canSaveProvider({ models: ['a', 'b'], enabled_models: ['gone'] })).toBe(false);
    expect(canSaveProvider({ models: ['a', 'b'], enabled_models: ['b'] })).toBe(true);
  });
});

describe('initialChecks', () => {
  const many = Array.from({ length: 30 }, (_, i) => `m${i}`);

  it('checks every model of a short list', () => {
    expect(initialChecks(['a', 'b'], [], null)).toEqual(['a', 'b']);
  });

  it('checks only the suggested model of a long list', () => {
    expect(initialChecks(many, [], 'm7')).toEqual(['m7']);
    expect(initialChecks(many, [], null)).toEqual([]);
    expect(initialChecks(many, [], 'not-listed')).toEqual([]);
  });

  it('keeps what was checked and is still listed', () => {
    expect(initialChecks(many, ['m3', 'gone'], 'm7')).toEqual(['m3']);
    expect(initialChecks(['a', 'b'], ['b'], null)).toEqual(['b']);
  });
});
