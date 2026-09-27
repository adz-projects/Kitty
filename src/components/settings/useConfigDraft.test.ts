import { describe, expect, it } from 'vitest';
import { configDiff, rebase } from './useConfigDraft';

describe('useConfigDraft helpers', () => {
  it('sends only the fields that changed, nested objects whole', () => {
    const base = {
      theme: 'light',
      hotkeys: ['Alt+Space'],
      summarizer: { enabled: true, model: 'a' },
    };
    const draft = { ...base, summarizer: { enabled: false, model: 'a' } };
    expect(configDiff(base, draft)).toEqual({ summarizer: { enabled: false, model: 'a' } });
    expect(configDiff(base, { ...base })).toEqual({});
    expect(configDiff({ ...base, x: 'k' }, { ...base, x: null })).toEqual({ x: null });
  });

  it('keeps unsaved edits over a config written elsewhere', () => {
    const base = { theme: 'light', show_artifacts: true };
    const draft = { theme: 'dark', show_artifacts: true };
    const fresh = { theme: 'light', show_artifacts: false };
    expect(rebase(base, draft, fresh)).toEqual({ theme: 'dark', show_artifacts: false });
  });
});
