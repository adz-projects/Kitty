import { describe, expect, it } from 'vitest';
import { importSummaryText, listNames, restartMessage } from './HubBanners';

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

  it('says when the engine is out of date, and who is holding it', () => {
    const update: [string, string] = ['2.9.1', '2.10.0'];
    expect(restartMessage(state({ reload_required: true, engine_update: update }), false)).toBe(
      'Kitty’s engine is out of date (2.9.1; this Kitty comes with 2.10.0). It updates when it restarts.'
    );
    const held = restartMessage(
      state({
        restart_pending: true,
        engine_update: update,
        blocked_by: [{ app_id: 'bacon', display_name: 'Bacon', reason: 'attached' }],
      }),
      false
    );
    expect(held).toContain('but Bacon is using it. It updates on its own');
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

describe('importSummaryText', () => {
  it('says what came across and what needs attention', () => {
    const text = importSummaryText({
      sessions: 12,
      sessions_skipped: 0,
      messages: 300,
      providers: 1,
      providers_skipped: 0,
      mcp_servers: 0,
      mcp_servers_skipped: 0,
      hitl_rules: 2,
      undecryptable_secrets: 1,
      pathway: 'imported',
    });
    expect(text).toContain('12 chats and 1 provider');
    expect(text).toContain('learned about you');
    expect(text).toContain("1 saved key couldn't be read");
  });
});
