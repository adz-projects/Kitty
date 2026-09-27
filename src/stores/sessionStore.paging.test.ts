import { describe, expect, it } from 'vitest';
import { mergeSessions, searchRows } from './sessionStore';
import type { SessionSummary } from '@/lib/types';

const row = (id: string, updatedAt: string): SessionSummary => ({
  sessionId: id,
  title: id,
  cwd: `/c/${id}`,
  updatedAt,
});

describe('session paging and search', () => {
  it('merges a page by id, newest first', () => {
    const merged = mergeSessions(
      [row('a', '2026-01-02'), row('b', '2026-01-01')],
      [row('b', '2026-01-03'), row('c', '2025-12-31')]
    );
    expect(merged.map((s) => s.sessionId)).toEqual(['b', 'a', 'c']);
  });

  it('turns search hits into rows, keeping what the loaded list knows', () => {
    const rows = searchRows(
      [
        { sessionId: 'a', title: 'A', snippet: 'match' },
        { sessionId: 'z', title: 'Old chat', snippet: null },
      ],
      [row('a', '2026-01-02')]
    );
    expect(rows[0]).toMatchObject({ sessionId: 'a', cwd: '/c/a', snippet: 'match' });
    expect(rows[1]).toMatchObject({ sessionId: 'z', title: 'Old chat', cwd: '' });
  });
});
