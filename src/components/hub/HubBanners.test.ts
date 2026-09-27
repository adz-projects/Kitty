import { describe, expect, it } from 'vitest';
import { listNames, restartMessage } from './HubBanners';

const state = (over: Partial<Parameters<typeof restartMessage>[0]> = {}) => ({
  reload_required: false,
  restart_pending: false,
  blocked_by: [],
  ...over,
});

describe('restartMessage', () => {
  it('says nothing when nothing is waiting', () => {
    expect(restartMessage(state(), false)).toBeNull();
  });

  it('names the apps in the way', () => {
    const msg = restartMessage(
      state({
        restart_pending: true,
        blocked_by: [
          { app_id: 'a', display_name: 'Research', reason: 'attached' },
          { app_id: 'a', display_name: 'Research', reason: 'active_turn' },
          { app_id: 'b', display_name: 'Notes', reason: 'attached' },
        ],
      }),
      false
    );
    expect(msg).toContain('Research and Notes are using it');
  });

  it('on Android, waits for the next start', () => {
    expect(restartMessage(state({ reload_required: true }), true)).toContain('next time');
  });
});

describe('listNames', () => {
  it('joins names in plain English', () => {
    expect(listNames(['A'])).toBe('A');
    expect(listNames(['A', 'B', 'C'])).toBe('A, B and C');
  });
});
