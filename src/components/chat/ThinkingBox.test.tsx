import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { ThinkingBox } from './ThinkingBox';
import { messageHasThinkingBox } from './MessageItem';
import type { Message } from '@/stores/chatStore';

function makeMessage(overrides: Partial<Message> = {}): Message {
  return {
    id: 'm1',
    role: 'assistant',
    text: '',
    reasoning: '',
    toolCalls: [],
    streaming: true,
    open: true,
    ...overrides,
  };
}

describe('messageHasThinkingBox', () => {
  it('returns false for user messages, superseded turns, and empty assistant turns', () => {
    expect(messageHasThinkingBox(makeMessage({ role: 'user', reasoning: 'x' }))).toBe(false);
    expect(messageHasThinkingBox(makeMessage({ superseded: true, reasoning: 'x' }))).toBe(false);
    expect(messageHasThinkingBox(makeMessage())).toBe(false);
  });

  it('returns true when an assistant turn has reasoning or non-visualization tool calls', () => {
    expect(messageHasThinkingBox(makeMessage({ reasoning: 'checking files' }))).toBe(true);
    expect(
      messageHasThinkingBox(
        makeMessage({
          toolCalls: [{ id: 't1', title: 'read_file', status: 'running' }],
        })
      )
    ).toBe(true);
  });
});

describe('ThinkingBox', () => {
  it('folds the live progress stage into the thinking summary while awaiting an answer', () => {
    const html = renderToStaticMarkup(
      <ThinkingBox
        reasoning="checking excerpts"
        toolCalls={[]}
        streaming={true}
        hasAnswer={false}
        progressStage="thinking"
      />
    );
    expect(html).toContain('reasoning-live');
    expect(html).toContain('Provider is thinking…');
    expect(html).toContain('live');
  });

  it('drops the live badge once the answer starts rendering', () => {
    const html = renderToStaticMarkup(
      <ThinkingBox
        reasoning="checking excerpts"
        toolCalls={[]}
        streaming={true}
        hasAnswer={true}
        progressStage="thinking"
      />
    );
    expect(html).not.toContain('reasoning-live');
    expect(html).not.toContain('Provider is thinking…');
  });
});
