import { describe, it, expect } from 'vitest';
import { findMatchingProvider } from './chatStore';
import type { ProviderView } from '@/lib/types';

/** Backs "reopen an old chat, Kitty switches to the provider it used"
    (`loadSession`): a session only carries goosed-level `providerId`/
    `modelId` (`session/list`'s `_meta`), which has to be mapped back to one
    of Kitty's own provider profiles — or found to have none, in which case
    the chat becomes read-only ("Chat concluded."). */

function profile(overrides: Partial<ProviderView>): ProviderView {
  return {
    id: 'p1',
    name: 'Test',
    provider_type: 'openrouter',
    base_url: 'https://openrouter.ai/api',
    models: ['deepseek/deepseek-v4-pro'],
    is_trusted: true,
    temperature: null,
    top_p: null,
    top_k: null,
    min_p: null,
    presence_penalty: null,
    frequency_penalty: null,
    max_tokens: null,
    context_length: null,
    strip_reasoning: false,
    supports_vision: false,
    system_prompt: null,
    prompt_idle_timeout_secs: null,
    parallel_slots: null,
    created_at: '',
    network_tier: 'remote',
    has_secret: true,
    active: false,
    ...overrides,
  };
}

describe('findMatchingProvider', () => {
  it('matches on provider type and a model present in the profile', () => {
    const p = profile({ id: 'p1' });
    const match = findMatchingProvider([p], 'openrouter', 'deepseek/deepseek-v4-pro');
    expect(match?.id).toBe('p1');
  });

  it('returns null when no profile has that provider type', () => {
    const p = profile({ id: 'p1', provider_type: 'anthropic', models: ['claude-sonnet-5'] });
    expect(findMatchingProvider([p], 'openrouter', 'deepseek/deepseek-v4-pro')).toBeNull();
  });

  it('returns null when the provider type matches but the model does not', () => {
    const p = profile({ id: 'p1', models: ['some-other-model'] });
    expect(findMatchingProvider([p], 'openrouter', 'deepseek/deepseek-v4-pro')).toBeNull();
  });

  it('returns null when the profile that used to match was deleted', () => {
    expect(findMatchingProvider([], 'openrouter', 'deepseek/deepseek-v4-pro')).toBeNull();
  });

  it('matches custom_openai profiles to goosed\'s "openai" providerId', () => {
    // Mirrors `goose_provider_name` in providers.rs: goosed's own session
    // metadata has no `custom_openai` concept — it only ever reports
    // "openai", since that's the underlying client Kitty routes through.
    const p = profile({ id: 'p1', provider_type: 'custom_openai', models: ['local-model'] });
    const match = findMatchingProvider([p], 'openai', 'local-model');
    expect(match?.id).toBe('p1');
  });

  it('does not match a custom_openai profile against a literal "custom_openai" providerId', () => {
    const p = profile({ id: 'p1', provider_type: 'custom_openai', models: ['local-model'] });
    expect(findMatchingProvider([p], 'custom_openai', 'local-model')).toBeNull();
  });

  it('first match wins when multiple profiles share type and model', () => {
    const a = profile({ id: 'p1' });
    const b = profile({ id: 'p2' });
    const match = findMatchingProvider([a, b], 'openrouter', 'deepseek/deepseek-v4-pro');
    expect(match?.id).toBe('p1');
  });

  it('matches on the Kitty profile id stored in session metadata', () => {
    // BigTiny stores the Kitty profile id in session metadata via
    // `set_session_provider`; `_meta.providerId` then carries that id rather
    // than a goosed type name, so findMatchingProvider must also match on it.
    const p = profile({ id: 'provider-abc', provider_type: 'openrouter' });
    const match = findMatchingProvider([p], 'provider-abc', 'deepseek/deepseek-v4-pro');
    expect(match?.id).toBe('provider-abc');
  });

  it('returns null when the stored profile id matches no profile', () => {
    const p = profile({ id: 'provider-abc', provider_type: 'openrouter' });
    expect(findMatchingProvider([p], 'provider-xyz', 'deepseek/deepseek-v4-pro')).toBeNull();
  });
});
