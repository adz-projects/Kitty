import { describe, it, expect, vi, beforeEach } from 'vitest';
import type { ApprovalNeededEvent, Recipe, SessionInfo } from '@/lib/types';

// Store-action tests for the WS8 chat-layer guards. chatStore calls ipc only
// inside actions (its `bindEvents`/subscription surface is never touched
// here), so a small, fully-mocked ipc is enough to exercise the actions.
vi.mock('@/lib/ipc', () => ({
  ipc: {
    newSession: vi.fn(),
    listProviders: vi.fn(),
    setMode: vi.fn(),
    setSessionMode: vi.fn(),
    sendPrompt: vi.fn(),
    setSessionPersonaOverride: vi.fn(),
    bindWindowSession: vi.fn(),
    respondPermission: vi.fn(),
    addRecipeExtension: vi.fn(),
    deleteSession: vi.fn(),
    loadSession: vi.fn(),
    isSessionBusy: vi.fn(),
    getSessionMode: vi.fn(),
    setSessionProvider: vi.fn(),
    cancelPrompt: vi.fn(),
    readFileAny: vi.fn(),
  },
}));

const { ipc } = await import('@/lib/ipc');
const { useChatStore } = await import('./chatStore');

const info = (sessionId: string, cwd = '/c'): SessionInfo => ({
  session_id: sessionId,
  cwd,
  current_mode: 'auto',
  available_modes: [],
  thinking_effort: null,
});

const approval: ApprovalNeededEvent = {
  session_id: 's1',
  tool_call_id: 't1',
  tool_call: { title: 'shell', kind: 'shell', rawInput: { command: 'x' } },
  options: [{ optionId: 'allow', name: 'Allow', kind: 'allow' }],
};

beforeEach(() => {
  vi.clearAllMocks();
  useChatStore.setState({
    sessionId: null,
    cwd: null,
    chatDir: null,
    sessionEpoch: 0,
    backgroundSession: null,
    backgroundTurnToast: null,
    title: null,
    mode: null,
    availableModes: [],
    thinkingEffort: null,
    creatingSession: false,
    messages: [],
    artifacts: [],
    droppedFiles: [],
    attachments: [],
    pendingImages: [],
    pendingAttachments: [],
    pendingApprovals: [],
    busy: false,
    sessionProviderId: null,
    sessionModelId: null,
    sessionConcluded: false,
    replaying: false,
    error: null,
    errorType: null,
    providerTier: null,
    providerHost: null,
    providerOffline: false,
    checkingConnection: false,
    isTrusted: false,
    model: 'test-model',
    providerName: null,
    stripReasoning: false,
    systemPrompt: null,
    modeOverride: null,
    savedApprovalMode: null,
    warning: null,
    compactionNotice: null,
    stopPhase: null,
    abandonedSession: null,
    loopSuspected: false,
    recipes: [],
    pendingRecipeCard: null,
    activeRecipeTurn: null,
    pendingForcedAnswer: null,
  });
  vi.mocked(ipc.newSession).mockResolvedValue(info('s1'));
  vi.mocked(ipc.listProviders).mockResolvedValue([]);
  vi.mocked(ipc.setMode).mockResolvedValue(undefined);
  vi.mocked(ipc.setSessionMode).mockResolvedValue(undefined);
  vi.mocked(ipc.sendPrompt).mockResolvedValue(undefined);
  vi.mocked(ipc.setSessionPersonaOverride).mockResolvedValue(undefined);
  vi.mocked(ipc.bindWindowSession).mockRejectedValue(new Error('best-effort'));
  vi.mocked(ipc.respondPermission).mockResolvedValue(undefined);
  vi.mocked(ipc.addRecipeExtension).mockResolvedValue(undefined);
  vi.mocked(ipc.getSessionMode).mockResolvedValue(null);
  vi.mocked(ipc.isSessionBusy).mockResolvedValue(false);
});

describe('chatStore send in-flight guard', () => {
  it('does not double-submit when send() is called twice in the same tick', async () => {
    const store = useChatStore.getState();
    const p1 = store.send('hello');
    const p2 = store.send('hello'); // same tick, before any await resolves
    await Promise.all([p1, p2]);

    expect(ipc.sendPrompt).toHaveBeenCalledTimes(1);
    expect(ipc.sendPrompt).toHaveBeenCalledWith('s1', 'hello', undefined);
  });

  it('releases the guard so a subsequent turn can send after the first settles', async () => {
    await useChatStore.getState().send('first');
    // A real completion event would clear busy; simulate it so the second
    // send here isn't legitimately blocked by the *busy* gate (the guard must
    // have already been released in the finally).
    useChatStore.setState({ busy: false, messages: [] });
    await useChatStore.getState().send('second');

    expect(ipc.sendPrompt).toHaveBeenCalledTimes(2);
  });
});

describe('chatStore sendWithRecipe guard', () => {
  const recipe: Recipe = {
    id: 'r1',
    slug: 'test',
    title: 'Test recipe',
    description: '',
    instructions: 'Be thorough.',
    prompt: null,
    version: '1.0',
    parameters: [],
    extensions: [],
    activities: [],
    is_builtin: false,
    created_at: '',
    max_reasoning_tokens: 1000,
  };

  it('invokes the recipe exactly once across a rapid double-click', async () => {
    const store = useChatStore.getState();
    const p1 = store.sendWithRecipe(recipe, 'go');
    const p2 = store.sendWithRecipe(recipe, 'go');
    await Promise.all([p1, p2]);

    expect(ipc.sendPrompt).toHaveBeenCalledTimes(1);
  });
});

describe('chatStore refreshProvider failure reset', () => {
  it('clears a stale isTrusted so the untrusted-provider warning still fires', async () => {
    useChatStore.setState({
      isTrusted: true,
      providerTier: 'remote',
      model: 'm',
      providerName: 'x',
    });
    vi.mocked(ipc.listProviders).mockRejectedValue(new Error('down'));

    await useChatStore.getState().refreshProvider();

    const s = useChatStore.getState();
    expect(s.isTrusted).toBe(false);
    expect(s.providerTier).toBe(null);
    expect(s.model).toBe(null);
  });
});

describe('chatStore forceStop / respondApproval', () => {
  it('forceStop clears pending approvals for the abandoned turn', () => {
    useChatStore.setState({
      sessionId: 's1',
      busy: true,
      stopPhase: 'forceable',
      pendingApprovals: [approval],
    });

    useChatStore.getState().forceStop();

    const s = useChatStore.getState();
    expect(s.busy).toBe(false);
    expect(s.pendingApprovals).toEqual([]);
  });

  it('respondApproval keeps the approval queued when the IPC round-trip fails', async () => {
    useChatStore.setState({ pendingApprovals: [approval], error: null });
    vi.mocked(ipc.respondPermission).mockRejectedValue(new Error('boom'));

    await useChatStore.getState().respondApproval('t1', 'allow');

    expect(useChatStore.getState().pendingApprovals).toHaveLength(1);
    expect(useChatStore.getState().error).toBe('Error: boom');
  });

  it('respondApproval removes the approval only after the IPC round-trip succeeds', async () => {
    useChatStore.setState({ pendingApprovals: [approval], error: null });

    await useChatStore.getState().respondApproval('t1', 'allow');

    expect(ipc.respondPermission).toHaveBeenCalledWith('t1', 'allow');
    expect(useChatStore.getState().pendingApprovals).toEqual([]);
  });
});

describe('chatStore session epoch', () => {
  it('newSession bumps the epoch and clears replaying (New Chat mid-replay)', async () => {
    useChatStore.setState({
      sessionId: 'old',
      cwd: '/old',
      busy: true,
      replaying: true,
      sessionEpoch: 5,
    });

    await useChatStore.getState().newSession();

    const s = useChatStore.getState();
    expect(s.sessionEpoch).toBe(6);
    expect(s.replaying).toBe(false);
    expect(s.sessionId).toBe('s1');
    // The busy session we left is remembered as a background turn.
    expect(s.backgroundSession).toEqual({ sessionId: 'old', cwd: '/old', title: null });
  });

  it('a stale loadSession replay cannot clobber a turn started after New Chat', async () => {
    let resolveLoad!: (v: SessionInfo) => void;
    vi.mocked(ipc.loadSession).mockImplementation(
      () => new Promise<SessionInfo>((res) => (resolveLoad = res))
    );

    // User opens session A (replay hangs open)…
    const loadP = useChatStore.getState().loadSession('A', '/a');
    expect(useChatStore.getState().sessionId).toBe('A');
    const epochA = useChatStore.getState().sessionEpoch;

    // …then clicks New Chat and immediately sends, so the new session is busy.
    await useChatStore.getState().newSession();
    expect(useChatStore.getState().sessionEpoch).toBe(epochA + 1);
    await useChatStore.getState().send('hi');
    expect(useChatStore.getState().busy).toBe(true);

    // The old replay finally completes…
    resolveLoad(info('A', '/a'));
    await loadP;

    const s = useChatStore.getState();
    // …and must NOT have flipped the still-active new turn back to idle.
    expect(s.sessionId).toBe('s1');
    expect(s.busy).toBe(true);
    expect(s.replaying).toBe(false);
  });
});
