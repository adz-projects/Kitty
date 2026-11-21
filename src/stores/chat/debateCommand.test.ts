import { describe, expect, it } from 'vitest';
import { parseDebateCommand } from './messageUtils';

describe('parseDebateCommand', () => {
  it('takes the instructions after /debate', () => {
    expect(parseDebateCommand('/debate three faculty, 3 rounds')).toBe('three faculty, 3 rounds');
    expect(parseDebateCommand('  /debate\nline one\nline two ')).toBe('line one\nline two');
  });

  it('accepts /debate alone', () => {
    expect(parseDebateCommand('/debate')).toBe('');
  });

  it('leaves anything else an ordinary message', () => {
    for (const text of ['/debates', '/debate-club', 'please /debate this', '/compact', '']) {
      expect(parseDebateCommand(text)).toBeNull();
    }
  });
});
