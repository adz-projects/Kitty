import { describe, expect, it } from 'vitest';
import { hasSomethingToSend, untrustedWarning } from './messageUtils';

const empty = { droppedFiles: [], attachments: [], pendingImages: [] };

describe('hasSomethingToSend', () => {
  it('sends text, or any attachment without text', () => {
    expect(hasSomethingToSend('  ', empty)).toBe(false);
    expect(hasSomethingToSend('hi', empty)).toBe(true);
    expect(hasSomethingToSend('', { ...empty, pendingImages: [{}] })).toBe(true);
    expect(hasSomethingToSend('', { ...empty, droppedFiles: [{}] })).toBe(true);
    expect(hasSomethingToSend('', { ...empty, attachments: [{}] })).toBe(true);
  });
});

describe('untrustedWarning', () => {
  it('warns only for an untrusted, non-local provider', () => {
    const remote = { providerTier: 'remote', isTrusted: false, providerHost: 'api.x.com' };
    expect(untrustedWarning('This image', remote)).toBe(
      "This image will be sent to api.x.com, which you haven't marked trusted."
    );
    expect(untrustedWarning('x', { ...remote, isTrusted: true })).toBeNull();
    expect(untrustedWarning('x', { ...remote, providerTier: 'local' })).toBeNull();
    expect(untrustedWarning('x', { ...remote, providerTier: null })).toBeNull();
  });
});
