import { describe, it, expect } from 'vitest';
import { isStragglerAssistantMessage, type Message } from './chatStore';

/** Backs the fix for a naked "thinking" box (tool calls with no answer text)
    trailing after a real response: a tool-call/delta notification and the
    turn's completion response travel through separate backend tasks, so a
    straggling event for the turn that just finished can arrive a moment
    after `chat://complete` already closed the message. */

const closedAssistant: Message = {
  id: '1',
  role: 'assistant',
  text: 'the answer',
  reasoning: '',
  toolCalls: [],
  streaming: false,
  open: false,
};

describe('isStragglerAssistantMessage', () => {
  it('is true for a closed assistant message when no turn is in flight', () => {
    expect(isStragglerAssistantMessage(closedAssistant, false)).toBe(true);
  });

  it('is false while a new turn is busy (avoids attaching to the wrong turn)', () => {
    expect(isStragglerAssistantMessage(closedAssistant, true)).toBe(false);
  });

  it('is false for a still-open (actively streaming) assistant message', () => {
    expect(isStragglerAssistantMessage({ ...closedAssistant, open: true }, false)).toBe(false);
  });

  it('is false for a user message', () => {
    expect(isStragglerAssistantMessage({ ...closedAssistant, role: 'user' }, false)).toBe(false);
  });

  it('is false when there is no last message', () => {
    expect(isStragglerAssistantMessage(undefined, false)).toBe(false);
  });
});
