import { describe, it, expect, vi, beforeEach } from 'vitest';
import type { OverlayDrainResult } from '@/lib/types';

const { handlers, ipc } = vi.hoisted(() => {
  const handlers: Record<string, (e: unknown) => void> = {};
  const ipc = {
    newSession: vi.fn(),
    loadSession: vi.fn(),
    setSessionProvider: vi.fn(),
    listProviders: vi.fn(),
    sendPrompt: vi.fn(),
    bindWindowSession: vi.fn(),
    isSessionBusy: vi.fn(),
    listSessionAllowedDirs: vi.fn(),
    drainOverlayRun: vi.fn(),
    listPendingApprovals: vi.fn(),
    listDirectory: vi.fn(),
    outputsSnapshot: vi.fn(),
  };
  return { handlers, ipc };
});

vi.mock('@/lib/ipc', () => {
  const names = [
    'onAdoptSession',
    'onChatError',
    'onClipboardAttach',
    'onCompaction',
    'onContextBudget',
    'onComplete',
    'onMessageDelta',
    'onEffortDetected',
    'onProviderActivated',
    'onProviderHealth',
    'onChatNotice',
    'onSessionRenamed',
    'onReasoningDelta',
    'onSessionDeleted',
    'onSessionsCleared',
    'onTidyDone',
    'onSessionTitle',
    'onSubagentStatus',
    'onToolCall',
    'onMemoryRecall',
    'onUserMessage',
    'onSuperseded',
    'onPendingApproval',
    'onApprovalResolved',
  ];
  const mod: Record<string, unknown> = { ipc, pickSavePath: vi.fn() };
  for (const n of names) {
    mod[n] = (h: (e: unknown) => void) => {
      handlers[n] = h;
      return Promise.resolve(() => {});
    };
  }
  return mod;
});

const { useChatStore } = await import('./chatStore');

beforeEach(() => {
  vi.clearAllMocks();
  useChatStore.setState({
    sessionId: null,
    cwd: null,
    chatDir: null,
    sessionEpoch: 0,
    messages: [],
    busy: false,
    error: null,
    errorType: null,
    title: null,
    thinkingEffort: null,
    sessionProviderId: null,
    sessionModelId: null,
  });
  vi.mocked(ipc.listProviders).mockResolvedValue([]);
  vi.mocked(ipc.bindWindowSession).mockResolvedValue(undefined);
  vi.mocked(ipc.listSessionAllowedDirs).mockResolvedValue({
    chat_dir: '/c',
    cwd: '/c',
    working_dirs: [],
    attached_paths: [],
  });
  vi.mocked(ipc.listPendingApprovals).mockResolvedValue([]);
});

describe('chatStore adoptOverlayRun', () => {
  it('replays drained events and events arriving mid-drain in exact order without drops', async () => {
    let resolveDrain!: (res: OverlayDrainResult) => void;
    vi.mocked(ipc.drainOverlayRun).mockImplementation(
      () => new Promise<OverlayDrainResult>((res) => (resolveDrain = res))
    );

    const adoptP = useChatStore.getState().adoptOverlayRun({
      session_id: 'ov1',
      cwd: '/c/chats/ov1',
      is_default_folder: true,
      provider_id: 'p1',
      model_id: 'm1',
    });

    await vi.waitFor(() => expect(ipc.drainOverlayRun).toHaveBeenCalledWith('ov1'));

    // Live delta and rename arrive while drainOverlayRun is still in flight.
    handlers.onMessageDelta({ session_id: 'ov1', text: ' world!' });
    handlers.onSessionRenamed({ sessionId: 'ov1', title: 'Manual Rename' });
    handlers.onComplete({ session_id: 'ov1', result: { stopReason: 'end_turn' } });

    resolveDrain({
      busy: true,
      events: [
        {
          event: 'chat://user-message',
          payload: { session_id: 'ov1', text: 'Say hello', attachments: [] },
        },
        {
          event: 'chat://reasoning-delta',
          payload: { session_id: 'ov1', text: 'thinking...' },
        },
        {
          event: 'chat://session-title',
          payload: { session_id: 'ov1', title: 'Auto Title' },
        },
        {
          event: 'chat://message-delta',
          payload: { session_id: 'ov1', text: 'Hello' },
        },
      ],
    });

    await adoptP;

    const s = useChatStore.getState();
    expect(s.sessionId).toBe('ov1');
    expect(s.busy).toBe(false);
    // Live rename queued mid-drain must win over older buffered auto-title.
    expect(s.title).toBe('Manual Rename');
    expect(s.messages).toHaveLength(2);
    expect(s.messages[0]).toMatchObject({ role: 'user', text: 'Say hello' });
    expect(s.messages[1]).toMatchObject({
      role: 'assistant',
      reasoning: 'thinking...',
      text: 'Hello world!',
      open: false,
    });
  });

  it('closes an already-completed overlay turn cleanly when drained.busy is false', async () => {
    vi.mocked(ipc.drainOverlayRun).mockResolvedValue({
      busy: false,
      events: [
        {
          event: 'chat://user-message',
          payload: { session_id: 'ov2', text: 'Quick question', attachments: [] },
        },
        {
          event: 'chat://message-delta',
          payload: { session_id: 'ov2', text: 'Quick answer.' },
        },
      ],
    });

    await useChatStore.getState().adoptOverlayRun({
      session_id: 'ov2',
      cwd: '/c/chats/ov2',
    });

    const s = useChatStore.getState();
    expect(s.busy).toBe(false);
    expect(s.messages[1]).toMatchObject({
      role: 'assistant',
      text: 'Quick answer.',
      open: false,
    });
  });

  it('falls back to loadSession when the backend overlay buffer already expired', async () => {
    vi.mocked(ipc.drainOverlayRun).mockResolvedValue({
      busy: false,
      events: [],
    });
    vi.mocked(ipc.loadSession).mockResolvedValue({
      session_id: 'ov3',
      cwd: '/c/chats/ov3',
      thinking_effort: null,
      is_default_folder: true,
      provider_id: null,
      model_id: null,
      busy: false,
      replay: [
        {
          event: 'chat://user-message',
          payload: { session_id: 'ov3', text: 'Persisted prompt', attachments: [] },
        },
        {
          event: 'chat://message-delta',
          payload: { session_id: 'ov3', text: 'Persisted reply.' },
        },
      ],
    });

    await useChatStore.getState().adoptOverlayRun({
      session_id: 'ov3',
      cwd: '/c/chats/ov3',
    });

    expect(ipc.loadSession).toHaveBeenCalledWith('ov3', '/c/chats/ov3');
    const s = useChatStore.getState();
    expect(s.busy).toBe(false);
    expect(s.messages).toHaveLength(2);
    expect(s.messages[1]).toMatchObject({
      role: 'assistant',
      text: 'Persisted reply.',
      open: false,
    });
  });

  it('routes chat://adopt-session with overlay_run: true through adoptOverlayRun', async () => {
    useChatStore.getState().bindEvents();
    vi.mocked(ipc.drainOverlayRun).mockResolvedValue({
      busy: true,
      events: [
        {
          event: 'chat://user-message',
          payload: { session_id: 'ov4', text: 'HITL prompt', attachments: [] },
        },
      ],
    });

    handlers.onAdoptSession({
      session_id: 'ov4',
      cwd: '/c/chats/ov4',
      is_default_folder: true,
      overlay_run: true,
    });

    await vi.waitFor(() => expect(ipc.drainOverlayRun).toHaveBeenCalledWith('ov4'));
    expect(ipc.loadSession).not.toHaveBeenCalled();
    expect(useChatStore.getState().messages[0]).toMatchObject({
      role: 'user',
      text: 'HITL prompt',
    });
  });
});
