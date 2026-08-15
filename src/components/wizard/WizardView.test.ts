import { describe, it, expect } from 'vitest';
import { androidSteps, stepsForPath } from './WizardView';

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

/** Android has no path fork: chat never runs locally there (D18), so
    "run models on this computer or use your own API key?" is a question with
    one real answer. The flow is instead the two things a phone actually
    needs, in order. */
describe('androidSteps', () => {
  it('is support models, then a provider, then done', () => {
    expect(androidSteps().map((s) => s.id)).toEqual(['support', 'apikey', 'done']);
  });

  it('never offers the local-vs-API-key fork', () => {
    expect(androidSteps().some((s) => s.id === 'path')).toBe(false);
  });

  /// `configure` sets a default context folder and a global hotkey. Android
  /// has neither, so a step that showed both would be entirely inert.
  it('drops the desktop-only configure step', () => {
    expect(androidSteps().some((s) => s.id === 'configure')).toBe(false);
  });

  /// The embedding model is not its own step here — it ships alongside the
  /// summarizer in `support`, which is the whole point of that screen.
  it('folds the embedding model into the support step', () => {
    expect(androidSteps().some((s) => s.id === 'embedding')).toBe(false);
    expect(androidSteps()[0].id).toBe('support');
  });
});
