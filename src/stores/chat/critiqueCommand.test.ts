import { describe, expect, it } from 'vitest';
import { answersCritiqueBrief, parseCritiqueCommand } from './messageUtils';
import type { Message } from './types';

describe('parseCritiqueCommand', () => {
  it('takes the options and instructions after /critique', () => {
    expect(parseCritiqueCommand('/critique genre=op-ed factcheck')).toBe('genre=op-ed factcheck');
    expect(parseCritiqueCommand('  /critique\nfor the dean ')).toBe('for the dean');
    expect(parseCritiqueCommand('/critique')).toBe('');
  });

  it('leaves other messages alone', () => {
    for (const text of ['/critiques', '/critique-me', 'please /critique this', '/debate', '']) {
      expect(parseCritiqueCommand(text)).toBeNull();
    }
  });
});

const msg = (role: 'user' | 'assistant', extra: Partial<Message> = {}): Message => ({
  id: Math.random().toString(36),
  role,
  text: '',
  reasoning: '',
  toolCalls: [],
  streaming: false,
  open: false,
  ...extra,
});

describe('answersCritiqueBrief', () => {
  const critiqueTurn = msg('user', {
    text: '/critique',
    attachedFiles: [{ name: 'Critique playbook', kind: 'critique' }],
  });

  it('is true right after the /critique turn and its brief', () => {
    expect(answersCritiqueBrief([critiqueTurn, msg('assistant')])).toBe(true);
  });

  it('is false once the brief has been answered, or with no critique', () => {
    expect(
      answersCritiqueBrief([critiqueTurn, msg('assistant'), msg('user'), msg('assistant')])
    ).toBe(false);
    expect(answersCritiqueBrief([msg('user'), msg('assistant')])).toBe(false);
    expect(answersCritiqueBrief([])).toBe(false);
  });
});
