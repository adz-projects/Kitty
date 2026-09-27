import { describe, expect, it } from 'vitest';
import { noticeText } from './notices';

describe('noticeText', () => {
  it('explains each failover reason and the step limit', () => {
    const base = { session_id: 's', message: null, model: 'gpt-x' } as const;
    expect(noticeText({ ...base, kind: 'failover', reason: 'pinned_unavailable' })).toContain(
      'gpt-x answered instead'
    );
    expect(noticeText({ ...base, kind: 'failover', reason: 'no_tool_support' })).toContain(
      'can’t use tools'
    );
    expect(noticeText({ ...base, kind: 'step_limit' })).toContain('step limit');
    expect(noticeText({ ...base, kind: 'failover', reason: null, message: 'engine text' })).toBe(
      'engine text'
    );
  });
});
