import { describe, it, expect } from 'vitest';
import { androidSteps, desktopSteps } from './WizardView';

/** The embedding-model step is offered whenever adaptive-pathway is enabled —
    its embeddings run on the in-process LiteRT engine regardless of which
    provider serves chat — and never otherwise. There's no local-vs-API-key
    fork any more: chat always goes through a connected provider (llama.cpp
    local chat is gone; LiteRT only does embeddings/summarization). */
describe('desktopSteps', () => {
  it('includes an embedding step when adaptive-pathway is enabled', () => {
    const ids = desktopSteps(true).map((s) => s.id);
    expect(ids).toContain('embedding');
  });

  it('omits the embedding step when adaptive-pathway is disabled', () => {
    const ids = desktopSteps(false).map((s) => s.id);
    expect(ids).not.toContain('embedding');
  });

  it('places the embedding step immediately before done', () => {
    const ids = desktopSteps(true).map((s) => s.id);
    const embeddingIdx = ids.indexOf('embedding');
    expect(ids[embeddingIdx + 1]).toBe('done');
  });

  it('never offers the retired local-vs-API-key fork or local model download', () => {
    const ids = desktopSteps(true).map((s) => s.id);
    expect(ids).not.toContain('path');
    expect(ids).not.toContain('model');
  });

  it('starts with connecting a provider', () => {
    expect(desktopSteps(false)[0].id).toBe('apikey');
  });
});

/** Android has no path fork either: chat never runs locally there (D18), so
    "run models on this computer or use your own API key?" is a question with
    one real answer. The flow is instead the two things a phone actually
    needs, in order. */
describe('androidSteps', () => {
  it('is support models, then a provider, then done', () => {
    expect(androidSteps().map((s) => s.id)).toEqual(['support', 'apikey', 'done']);
  });

  it('never offers the retired local-vs-API-key fork', () => {
    // 'path' isn't even a member of StepId any more — the fork is gone at the
    // type level, not just hidden at runtime.
    expect(androidSteps().some((s) => (s.id as string) === 'path')).toBe(false);
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
