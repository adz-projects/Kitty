import { describe, it, expect } from 'vitest';
import { buildStrippedTranscript, stripPromptPreamble } from './chatStore';
import type { Message } from './chatStore';

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

/** Sentinel-anchored strip (815bugs #42): the send-time wrapper is exactly
    `<transcript>\n\n[sentinel]\n\nUser: <text>`, so the replay strip anchors
    on that exact boundary. The pre-sentinel `lastIndexOf('\n\nUser: ')`
    heuristic could match inside the user's OWN message and eat its head. */
describe('stripPromptPreamble transcript sentinel', () => {
  const msg = (role: 'user' | 'assistant', text: string): Message => ({
    id: role + text.length,
    role,
    text,
    reasoning: '',
    toolCalls: [],
    streaming: false,
    open: false,
  });
  // Mirror of doSend's send-time construction.
  const wrap = (prior: Message[], userText: string) =>
    `${buildStrippedTranscript(prior)}\n\nUser: ${userText}`;

  it('round-trips the exact user text out of a freshly built wrapper', () => {
    const wrapped = wrap(
      [msg('user', 'first question'), msg('assistant', 'first answer')],
      'now what?'
    );
    expect(stripPromptPreamble(wrapped)).toBe('now what?');
  });

  it('preserves a user message that itself contains "\\n\\nUser: "', () => {
    const userText = 'look at this log:\n\nUser: foo\n\nAssistant: bar\n\nwhat does it mean?';
    const wrapped = wrap(
      [msg('user', 'earlier turn'), msg('assistant', 'earlier answer')],
      userText
    );
    expect(stripPromptPreamble(wrapped)).toBe(userText);
  });

  it('falls back to the legacy last-marker heuristic for pre-sentinel wrappers', () => {
    const wrapped =
      'Continuing the conversation below. Earlier reasoning/thinking has been omitted ' +
      'to keep this response focused.\n\n' +
      'User: earlier turn\n\n' +
      'Assistant: earlier answer\n\n' +
      'User: current message';
    expect(stripPromptPreamble(wrapped)).toBe('current message');
  });
});
