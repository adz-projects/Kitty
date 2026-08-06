import { describe, expect, it } from 'vitest';
import { stripInternalMarkers } from './chatStore';

describe('stripInternalMarkers', () => {
  it('leaves a normal message untouched', () => {
    expect(stripInternalMarkers('just a normal message')).toBe('just a normal message');
  });

  it('strips an injected recall block and its content', () => {
    const text =
      '[Earlier context from this session]\n' +
      'user: Keep the API key out of the summary.\n' +
      'assistant: Understood.\n\n' +
      'Now answer my question.';
    expect(stripInternalMarkers(text)).toBe('Now answer my question.');
  });

  it('strips a consolidated-project-memory block', () => {
    const text =
      'Hi there\n' +
      '[CONSOLIDATED PROJECT MEMORY]\n' +
      'key: invoice pipeline is rate-limited\n\n' +
      'continue';
    expect(stripInternalMarkers(text)).toBe('Hi there\ncontinue');
  });

  it('strips each marker block in turn', () => {
    const text =
      '[Adaptive Pathway hints]\n' +
      'hint: check the schema first\n\n' +
      '[Earlier context from this session]\n' +
      'user: earlier note\n\n' +
      'tail';
    expect(stripInternalMarkers(text)).toBe('tail');
  });
});
