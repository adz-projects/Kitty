import { describe, it, expect } from 'vitest';
import { modelLabel, modelTitle, priceHint } from './models';
import type { ModelInfo } from './types';

function info(over: Partial<ModelInfo>): ModelInfo {
  return {
    provider_id: 'p',
    provider_name: 'OpenRouter',
    provider_type: 'openrouter',
    model: 'x/y',
    card_id: 'c',
    name: 'X: Y',
    enabled: true,
    vision: false,
    in_catalog: true,
    created: null,
    price_out_per_m: null,
    price_in_per_m: null,
    price_tier: null,
    intelligence: null,
    context_length: null,
    ...over,
  };
}

describe('model labels', () => {
  it('adds the price band when known', () => {
    expect(modelLabel(info({ price_tier: '$$' }))).toBe('X: Y · $$');
    expect(modelLabel(info({}))).toBe('X: Y');
  });

  it('says the exact price', () => {
    expect(priceHint(info({}))).toBeNull();
    expect(priceHint(info({ price_out_per_m: 0 }))).toBe('Free');
    expect(priceHint(info({ price_out_per_m: 15 }))).toBe('$15.00 per million output tokens');
    expect(priceHint(info({ price_out_per_m: 15, price_in_per_m: 3 }))).toBe(
      '$3.00 in / $15.00 out per million tokens'
    );
  });

  it('puts id, price and intelligence in the hover text', () => {
    expect(modelTitle(info({ price_out_per_m: 2, intelligence: 61.4 }))).toBe(
      'x/y — $2.00 per million output tokens — Intelligence index 61'
    );
  });
});
