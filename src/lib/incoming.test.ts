import { describe, expect, it, vi } from 'vitest';

vi.mock('./ipc', () => ({ ipc: { takeIncoming: vi.fn() } }));

const { shareText } = await import('./incoming');

describe('shareText', () => {
  it('puts a subject above the text, without repeating it', () => {
    expect(shareText({ subject: 'Title', text: 'https://x.com' })).toBe('Title\n\nhttps://x.com');
    expect(shareText({ subject: 'Title', text: 'Title — https://x.com' })).toBe(
      'Title — https://x.com'
    );
    expect(shareText({ subject: '', text: ' hi ' })).toBe('hi');
    expect(shareText({ subject: 'Only', text: '' })).toBe('Only');
  });
});
