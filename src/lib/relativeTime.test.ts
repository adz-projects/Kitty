import { describe, expect, it } from 'vitest';
import { parseEngineTime, relativeTime } from './relativeTime';

const now = Date.parse('2026-09-27T12:00:00Z');

describe('relativeTime', () => {
  it('reads the engine timestamps as UTC', () => {
    expect(parseEngineTime('2026-09-27 11:00:00')).toBe(Date.parse('2026-09-27T11:00:00Z'));
    expect(parseEngineTime('')).toBeNull();
  });

  it('says how long ago in plain words', () => {
    expect(relativeTime('2026-09-27 11:59:30', now)).toBe('just now');
    expect(relativeTime('2026-09-27 11:45:00', now)).toBe('15 min ago');
    expect(relativeTime('2026-09-27 09:00:00', now)).toBe('3 h ago');
    expect(relativeTime('2026-09-26 10:00:00', now)).toBe('yesterday');
    expect(relativeTime('2026-09-23 12:00:00', now)).toBe('4 days ago');
    expect(relativeTime('garbage', now)).toBe('');
  });
});
