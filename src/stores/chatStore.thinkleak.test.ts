import { describe, it, expect } from 'vitest';
import { splitLeakedThinkTag } from './chatStore';

/** Backs the fix for a real, observed local-model failure mode: the model's
    own literal `<think>...</think>` reasoning tags sometimes get misclassified
    partway through by goosed/Ollama, so the tail of the thinking — including
    the model's own closing tag — renders in the visible answer bubble instead
    of the collapsible thinking box. Confirmed via a real captured transcript
    showing "(End of thought process)\n</think>" as literal text in the
    rendered answer, immediately followed by the real response. */

describe('splitLeakedThinkTag', () => {
  it('returns null when there is no leaked tag', () => {
    expect(splitLeakedThinkTag('Just a normal answer with no tags.')).toBeNull();
  });

  it('splits a trailing leaked close tag from the real answer', () => {
    const text = '(End of thought process)\n</think>\nThe real answer starts here.';
    const result = splitLeakedThinkTag(text);
    expect(result).not.toBeNull();
    expect(result!.reasoning).toBe('(End of thought process)');
    expect(result!.text).toBe('The real answer starts here.');
  });

  it('strips a leading open tag too, when the whole block leaked', () => {
    const text = '<think>\nPlanning out loud here.\n</think>\nHere is the answer.';
    const result = splitLeakedThinkTag(text);
    expect(result).not.toBeNull();
    expect(result!.reasoning).toBe('Planning out loud here.');
    expect(result!.text).toBe('Here is the answer.');
  });

  it('splits at the first occurrence when the tag appears once', () => {
    const text = 'Some thinking.</think>Answer part one. More answer.';
    const result = splitLeakedThinkTag(text);
    expect(result).not.toBeNull();
    expect(result!.reasoning).toBe('Some thinking.');
    expect(result!.text).toBe('Answer part one. More answer.');
  });

  it('handles an empty remainder after the tag (tag at the very end)', () => {
    const text = 'All thinking, no answer yet.</think>';
    const result = splitLeakedThinkTag(text);
    expect(result).not.toBeNull();
    expect(result!.reasoning).toBe('All thinking, no answer yet.');
    expect(result!.text).toBe('');
  });
});
