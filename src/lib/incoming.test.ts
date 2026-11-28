import { describe, expect, it, vi } from 'vitest';

const loadSessionMock = vi.fn().mockResolvedValue(undefined);
const newSessionMock = vi.fn().mockResolvedValue(undefined);
const gotoMock = vi.fn();

vi.mock('./ipc', () => ({ ipc: { takeIncoming: vi.fn() } }));
vi.mock('@/stores/chatStore', () => ({
  useChatStore: {
    getState: () => ({
      sessionId: 'other',
      loadSession: loadSessionMock,
      newSession: newSessionMock,
      addDroppedPaths: vi.fn(),
      setComposerPrefill: vi.fn(),
    }),
    setState: vi.fn(),
  },
}));
vi.mock('@/stores/routeStore', () => ({
  useRouteStore: {
    getState: () => ({ goto: gotoMock }),
  },
}));

const { shareText, handleIncoming } = await import('./incoming');

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

describe('handleIncoming', () => {
  it('opens the target chat session from a tapped notification', async () => {
    await handleIncoming({ kind: 'open_chat', session_id: 's1' });
    expect(gotoMock).toHaveBeenCalledWith('chat');
    expect(loadSessionMock).toHaveBeenCalledWith('s1', '');
  });
});
