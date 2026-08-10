import { describe, it, expect } from 'vitest';
import { stepsForPath } from './WizardView';

/** The embedding-model step must appear on BOTH wizard paths when
    adaptive-pathway is enabled (it needs Ollama purely for embeddings,
    regardless of chat provider), and never appear when disabled. */
describe('stepsForPath', () => {
  it('includes an embedding step on the local path when adaptive-pathway is enabled', () => {
    const ids = stepsForPath('local', true).map((s) => s.id);
    expect(ids).toContain('embedding');
  });

  it('includes an embedding step on the api-key path when adaptive-pathway is enabled', () => {
    const ids = stepsForPath('api-key', true).map((s) => s.id);
    expect(ids).toContain('embedding');
  });

  it('omits the embedding step on the local path when adaptive-pathway is disabled', () => {
    const ids = stepsForPath('local', false).map((s) => s.id);
    expect(ids).not.toContain('embedding');
  });

  it('omits the embedding step on the api-key path when adaptive-pathway is disabled', () => {
    const ids = stepsForPath('api-key', false).map((s) => s.id);
    expect(ids).not.toContain('embedding');
  });

  it('places the embedding step immediately before done on both paths', () => {
    for (const path of ['local', 'api-key'] as const) {
      const ids = stepsForPath(path, true).map((s) => s.id);
      const embeddingIdx = ids.indexOf('embedding');
      expect(ids[embeddingIdx + 1]).toBe('done');
    }
  });
});
