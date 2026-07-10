import { describe, it, expect } from 'vitest';
import { stripPromptPreamble } from './chatStore';

/** Backs the fix for the leaked system-prompt/strip-reasoning wrapper on
    session replay (Round-7): goosed stores exactly what was transmitted, so a
    resumed session's first turn would otherwise show the raw wrapper in the
    chat bubble. */

describe('stripPromptPreamble', () => {
  it('strips a system-prompt wrapper and returns the original user text', () => {
    const wrapped = '<system>\nYou are a capable assistant.\n</system>\n\nHello there';
    expect(stripPromptPreamble(wrapped)).toBe('Hello there');
  });

  it('strips a multi-line system-prompt wrapper', () => {
    const wrapped = '<system>\nLine one.\nLine two.\nLine three.\n</system>\n\nActual message';
    expect(stripPromptPreamble(wrapped)).toBe('Actual message');
  });

  it('strips a strip-reasoning transcript wrapper, keeping only the final user turn', () => {
    const wrapped =
      'Continuing the conversation below. Earlier reasoning/thinking has been omitted ' +
      'to keep this response focused.\n\n' +
      'User: first question\n\n' +
      'Assistant: first answer\n\n' +
      'User: current message';
    expect(stripPromptPreamble(wrapped)).toBe('current message');
  });

  it('strips a transcript wrapper even when the final message itself contains "User: "', () => {
    const wrapped =
      'Continuing the conversation below. Earlier reasoning/thinking has been omitted ' +
      'to keep this response focused.\n\n' +
      'User: earlier turn\n\n' +
      'User: can you explain what "User: " means in a chat log?';
    expect(stripPromptPreamble(wrapped)).toBe('can you explain what "User: " means in a chat log?');
  });

  it('returns plain text unchanged (no wrapper present)', () => {
    expect(stripPromptPreamble('just a normal message')).toBe('just a normal message');
  });

  it('returns text unchanged when it merely mentions <system> without the full wrapper shape', () => {
    const text = 'Can you explain what a <system> prompt is?';
    expect(stripPromptPreamble(text)).toBe(text);
  });
});
