import { describe, expect, it } from 'vitest';
import { sameLearningModel } from './LearningModel';

describe('sameLearningModel', () => {
  it('compares kinds, and provider choices by provider and model', () => {
    expect(sameLearningModel({ kind: 'auto' }, { kind: 'auto' })).toBe(true);
    expect(sameLearningModel({ kind: 'auto' }, { kind: 'chat' })).toBe(false);
    const a = { kind: 'provider', provider_id: 'or', model: 'm1' } as const;
    expect(sameLearningModel(a, { ...a })).toBe(true);
    expect(sameLearningModel(a, { ...a, model: 'm2' })).toBe(false);
  });
});
