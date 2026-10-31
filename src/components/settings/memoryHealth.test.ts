import { describe, expect, it } from 'vitest';
import { describeAge, isStalled, parseEngineTime } from './memoryHealth';

const NOW = Date.parse('2026-10-31T12:00:00Z');

describe('memory health helpers', () => {
  it('reads both engine timestamp shapes', () => {
    expect(parseEngineTime('2026-10-12T14:27:19Z')).toBe(Date.parse('2026-10-12T14:27:19Z'));
    expect(parseEngineTime('2026-10-04T12:25:25.187486100+00:00')).toBe(
      Date.parse('2026-10-04T12:25:25.187Z')
    );
    expect(parseEngineTime(null)).toBeNull();
    expect(parseEngineTime('not a time')).toBeNull();
  });

  it('calls a waiting backlog with no recent work a stall', () => {
    expect(isStalled(1072, '2026-10-12T14:27:19Z', NOW)).toBe(true);
    expect(isStalled(5, null, NOW)).toBe(true);
    expect(isStalled(5, '2026-10-31T09:00:00Z', NOW)).toBe(false);
  });

  it('never calls an empty backlog a stall', () => {
    expect(isStalled(0, '2026-01-01T00:00:00Z', NOW)).toBe(false);
    expect(isStalled(0, null, NOW)).toBe(false);
  });

  it('describes the age of the last run', () => {
    expect(describeAge(null, NOW)).toBe('never');
    expect(describeAge('2026-10-31T11:30:00Z', NOW)).toBe('within the last hour');
    expect(describeAge('2026-10-31T09:00:00Z', NOW)).toBe('3 hours ago');
    expect(describeAge('2026-10-12T12:00:00Z', NOW)).toBe('19 days ago');
  });
});
