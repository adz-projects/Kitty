import { describe, expect, it, vi, beforeEach } from 'vitest';

import type { ProviderView } from '@/lib/types';

/** A delegate is hosted by `subagent_pick::choose_host`, which routinely picks
    a different provider AND model from the session that started it. Two things
    used to make a watch window lie about that:
 *
 *  - `spectateSession` loaded the delegate's transcript with no provider/model,
 *    so the badge fell back to the globally active profile — the MAIN model —
 *    over a transcript the main model had no part in;
 *  - `refreshProvider` rendered `models[0]` rather than the session's own model,
 *    so any session pinned to anything but the head of the list was mislabelled
 *    even when the profile resolved correctly.
 *
 *  And a watch window must never WRITE: it is a read-only view of a delegate
 *  that is running right now, so stamping its provider config mid-run is a
 *  write nobody asked for. */

const setSessionProvider = vi.fn(async () => {});
const providers: ProviderView[] = [
  {
    id: 'p-main',
    name: 'Main',
    provider_type: 'openrouter',
    base_url: 'https://openrouter.ai/api/v1',
    models: ['big-model', 'small-model'],
    active: true,
    network_tier: 'remote',
    is_trusted: false,
    strip_reasoning: false,
    supports_vision: false,
    accepts_images: null,
    system_prompt: null,
  } as unknown as ProviderView,
];

vi.mock('@/lib/ipc', () => ({
  ipc: {
    listProviders: vi.fn(async () => providers),
    setSessionProvider: (...a: unknown[]) => setSessionProvider(...(a as [])),
    loadSession: vi.fn(async (session_id: string, cwd: string) => ({
      session_id,
      cwd,
      current_mode: 'approve',
      available_modes: [],
      thinking_effort: null,
      is_default_folder: false,
      provider_id: null,
      model_id: null,
    })),
    bindWindowSession: vi.fn(async () => {}),
    isSessionBusy: vi.fn(async () => false),
    listSessionAllowedDirs: vi.fn(async () => []),
    listSessionGrants: vi.fn(async () => []),
    listDir: vi.fn(async () => []),
  },
}));

const { useChatStore } = await import('./chatStore');

describe('watching a specialist labels it with the delegate’s own host', () => {
  beforeEach(() => {
    setSessionProvider.mockClear();
  });

  it('shows the delegate’s model, not the main one, and writes nothing', async () => {
    await useChatStore.getState().spectateSession('child-1', 'researcher', 'p-main', 'small-model');

    const s = useChatStore.getState();
    expect(s.spectating).toBe('researcher');
    expect(s.sessionModelId).toBe('small-model');
    expect(s.model).toBe('small-model');
    expect(setSessionProvider).not.toHaveBeenCalled();
  });

  it('falls back to the provider’s first model only when the session has none', async () => {
    useChatStore.setState({ spectating: null });
    await useChatStore.getState().loadSession('plain-1', '');
    expect(useChatStore.getState().model).toBe('big-model');
  });
});
