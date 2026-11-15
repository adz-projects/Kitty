import { describe, it, expect, vi, beforeEach } from 'vitest';
import type { FileEntry } from '@/lib/types';

// The output cards: a live turn's come from a before/after listing of the
// chat's `outputs` folder, a resumed chat's from the replayed tool calls.
// `on*` listeners are captured so the test can fire the events itself.
const { handlers, ipc } = vi.hoisted(() => {
  const handlers: Record<string, (e: unknown) => void> = {};
  const ipc = {
    newSession: vi.fn(),
    listProviders: vi.fn(),
    sendPrompt: vi.fn(),
    bindWindowSession: vi.fn(),
    isSessionBusy: vi.fn(),
    listDirectory: vi.fn(),
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
    'onSessionTitle',
    'onSubagentStatus',
    'onToolCall',
    'onUserMessage',
    'onSuperseded',
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
const { useArtifactsStore } = await import('./artifactsStore');

const file = (name: string, over: Partial<FileEntry> = {}): FileEntry => ({
  name,
  path: `/c/outputs/${name}`,
  size: 2048,
  modified: 100,
  is_dir: false,
  ...over,
});

const lastMsg = () => {
  const m = useChatStore.getState().messages;
  return m[m.length - 1];
};

const flush = () => new Promise((r) => setTimeout(r, 0));

const assistant = (text = 'Done.') => ({
  id: 'a1',
  role: 'assistant' as const,
  text,
  reasoning: '',
  toolCalls: [],
  streaming: true,
  open: true,
});

beforeEach(() => {
  vi.clearAllMocks();
  useChatStore.setState({
    sessionId: 's1',
    cwd: '/c',
    busy: false,
    messages: [],
    replaying: false,
    abandonedSession: null,
    error: null,
  });
  useArtifactsStore.setState({
    open: false,
    unseen: false,
    hasPane: false,
    listingCwd: null,
    entries: [],
  });
  ipc.listProviders.mockResolvedValue([]);
  ipc.sendPrompt.mockResolvedValue(undefined);
  ipc.bindWindowSession.mockRejectedValue(new Error('best-effort'));
  ipc.isSessionBusy.mockResolvedValue(false);
  useChatStore.getState().bindEvents();
});

const complete = (sid = 's1') => handlers.onComplete({ session_id: sid, result: { usage: {} } });

describe('output cards at the end of a live turn', () => {
  it('attaches a card for what the turn made, and flags the drawer', async () => {
    ipc.listDirectory.mockResolvedValueOnce([file('old.txt')]);
    await useChatStore.getState().send('make me a report');
    ipc.listDirectory.mockResolvedValue([file('old.txt'), file('report.docx')]);
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    const last = lastMsg();
    expect(last?.outputs?.map((o) => o.name)).toEqual(['report.docx']);
    expect(last?.outputs?.[0]).toMatchObject({ isDir: false, size: 2048 });
    expect(useArtifactsStore.getState().unseen).toBe(true);
    // The drawer's listing is refreshed from the same read.
    expect(useArtifactsStore.getState().listingCwd).toBe('/c');
  });

  it('counts a changed file, and a new folder as one entry', async () => {
    ipc.listDirectory.mockResolvedValueOnce([file('a.csv')]);
    await useChatStore.getState().send('go');
    ipc.listDirectory.mockResolvedValue([
      file('a.csv', { modified: 200 }),
      file('pdfs', { is_dir: true }),
    ]);
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    const names = lastMsg()?.outputs?.map((o) => o.name);
    expect(names?.sort()).toEqual(['a.csv', 'pdfs']);
  });

  it('does nothing when the turn left outputs as it found them', async () => {
    ipc.listDirectory.mockResolvedValue([file('a.csv')]);
    await useChatStore.getState().send('just chat');
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    expect(lastMsg()?.outputs).toBeUndefined();
    expect(useArtifactsStore.getState().unseen).toBe(false);
  });

  it('does not flag a drawer that is already open', async () => {
    ipc.listDirectory.mockResolvedValueOnce([]);
    await useChatStore.getState().send('go');
    ipc.listDirectory.mockResolvedValue([file('x.txt')]);
    useArtifactsStore.setState({ open: true });
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    expect(lastMsg()?.outputs).toHaveLength(1);
    expect(useArtifactsStore.getState().unseen).toBe(false);
  });

  it('attaches nothing if the window moved to another chat meanwhile', async () => {
    ipc.listDirectory.mockResolvedValueOnce([]);
    await useChatStore.getState().send('go');
    ipc.listDirectory.mockResolvedValue([file('x.txt')]);
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    useChatStore.setState({ sessionId: 's2', cwd: '/d', messages: [] });
    await flush();

    expect(useChatStore.getState().messages).toEqual([]);
    expect(useArtifactsStore.getState().unseen).toBe(false);
  });

  it('also cards a turn that failed after writing something', async () => {
    ipc.listDirectory.mockResolvedValueOnce([]);
    await useChatStore.getState().send('go');
    ipc.listDirectory.mockResolvedValue([file('partial.md')]);
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant('Working…')] }));

    handlers.onChatError({ session_id: 's1', message: 'boom' });
    await flush();

    expect(lastMsg()?.outputs?.[0].name).toBe('partial.md');
  });
});

describe('output cards rebuilt on resume', () => {
  const toolCall = (path: string, id = 't1') => ({
    session_id: 's1',
    phase: 'tool_call',
    update: {
      toolCallId: id,
      title: 'write',
      rawInput: { path },
      _meta: { goose: { toolCall: { toolName: 'write' } } },
    },
  });

  it('puts a card on the message that made the call, without flagging the drawer', () => {
    useChatStore.setState({ replaying: true });
    handlers.onToolCall(toolCall('outputs/report.docx'));

    const last = lastMsg();
    expect(last?.outputs?.map((o) => o.name)).toEqual(['report.docx']);
    expect(useArtifactsStore.getState().unseen).toBe(false);
  });

  it('uses the top-level folder for a file inside a subfolder, once per message', () => {
    useChatStore.setState({ replaying: true });
    handlers.onToolCall(toolCall('outputs/pdfs/a.pdf', 't1'));
    handlers.onToolCall(toolCall('outputs/pdfs/b.pdf', 't2'));

    const outputs = lastMsg()?.outputs;
    expect(outputs).toHaveLength(1);
    expect(outputs?.[0]).toMatchObject({ name: 'pdfs', isDir: true });
  });

  it('ignores files outside outputs', () => {
    useChatStore.setState({ replaying: true });
    handlers.onToolCall(toolCall('scratch/tmp.py'));

    expect(lastMsg()?.outputs).toBeUndefined();
  });

  it('does not card a live tool call (the end-of-turn listing does that)', () => {
    handlers.onToolCall(toolCall('outputs/report.docx'));

    expect(lastMsg()?.outputs).toBeUndefined();
  });
});
