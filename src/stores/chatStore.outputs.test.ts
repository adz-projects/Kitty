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

// A live turn reads `outputs` twice: a recursive snapshot when it begins (the
// diff's "before") and, when it ends, a snapshot ("after") plus a flat listing
// for the drawer. The snapshot entries have the same shape as `file()`; only
// their values matter to the diff.
const mockOutputs = (before: FileEntry[], after: FileEntry[]) => {
  ipc.outputsSnapshot.mockResolvedValueOnce(before);
  ipc.outputsSnapshot.mockResolvedValue(after);
  ipc.listDirectory.mockResolvedValue(after);
};

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
    mockOutputs([file('old.txt')], [file('old.txt'), file('report.docx')]);
    await useChatStore.getState().send('make me a report');
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
    mockOutputs(
      [file('a.csv')],
      [file('a.csv', { modified: 200 }), file('pdfs', { is_dir: true })]
    );
    await useChatStore.getState().send('go');
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    const names = lastMsg()?.outputs?.map((o) => o.name);
    expect(names?.sort()).toEqual(['a.csv', 'pdfs']);
  });

  it('does nothing when the turn left outputs as it found them', async () => {
    mockOutputs([file('a.csv')], [file('a.csv')]);
    await useChatStore.getState().send('just chat');
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    expect(lastMsg()?.outputs).toBeUndefined();
    expect(useArtifactsStore.getState().unseen).toBe(false);
  });

  it('does not flag a drawer that is already open', async () => {
    mockOutputs([], [file('x.txt')]);
    await useChatStore.getState().send('go');
    useArtifactsStore.setState({ open: true });
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    expect(lastMsg()?.outputs).toHaveLength(1);
    expect(useArtifactsStore.getState().unseen).toBe(false);
  });

  it('attaches nothing if the window moved to another chat meanwhile', async () => {
    mockOutputs([], [file('x.txt')]);
    await useChatStore.getState().send('go');
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    useChatStore.setState({ sessionId: 's2', cwd: '/d', messages: [] });
    await flush();

    expect(useChatStore.getState().messages).toEqual([]);
    expect(useArtifactsStore.getState().unseen).toBe(false);
    // The late completion must not clobber the new chat's listing either.
    expect(useArtifactsStore.getState().listingCwd).toBeNull();
  });

  it('also cards a turn that failed after writing something', async () => {
    mockOutputs([], [file('partial.md')]);
    await useChatStore.getState().send('go');
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant('Working…')] }));

    handlers.onChatError({ session_id: 's1', message: 'boom' });
    await flush();

    expect(lastMsg()?.outputs?.[0].name).toBe('partial.md');
  });

  it('flags the drawer even when the turn ended before any answer', async () => {
    mockOutputs([], [file('x.txt')]);
    await useChatStore.getState().send('go');
    // No assistant message was ever produced — the turn errored out early. The
    // new output must still flag the drawer, even though no card can attach.
    handlers.onChatError({ session_id: 's1', message: 'boom' });
    await flush();

    expect(useArtifactsStore.getState().unseen).toBe(true);
    expect(lastMsg()?.role).toBe('user');
    expect(lastMsg()?.outputs).toBeUndefined();
  });

  it('cards a folder whose nested file was edited (aggregate signature changed)', async () => {
    // The recursive snapshot reports the folder's whole-subtree size/mtime, so a
    // nested edit moves the top-level entry's signature even though the folder
    // itself was not created or renamed.
    mockOutputs(
      [file('pdfs', { is_dir: true, size: 100, modified: 100 })],
      [file('pdfs', { is_dir: true, size: 150, modified: 200 })]
    );
    await useChatStore.getState().send('update the report');
    useChatStore.setState((s) => ({ messages: [...s.messages, assistant()] }));

    complete();
    await flush();

    expect(lastMsg()?.outputs?.map((o) => o.name)).toEqual(['pdfs']);
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
    expect(last?.outputsCwd).toBe('/c');
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
