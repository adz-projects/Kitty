import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@/lib/ipc', () => ({
  ipc: {
    listSessions: vi.fn(),
    deleteSession: vi.fn(),
    renameSession: vi.fn(),
    listFolders: vi.fn(),
    createFolder: vi.fn(),
    renameFolder: vi.fn(),
    deleteFolder: vi.fn(),
    assignSessionFolder: vi.fn(),
  },
}));

const { ipc } = await import('@/lib/ipc');
const { useSessionStore, UNCATEGORIZED } = await import('./sessionStore');

function rawSession(sessionId: string, title: string, cwd: string, updatedAt: string) {
  return { sessionId, title, cwd, updatedAt };
}

beforeEach(() => {
  vi.clearAllMocks();
  useSessionStore.setState({
    sessions: [],
    loading: false,
    query: '',
    folders: [],
    assignments: {},
  });
  vi.mocked(ipc.listFolders).mockResolvedValue({ folders: [], assignments: {} });
});

describe('sessionStore.refresh', () => {
  it('loads and sorts sessions by updatedAt descending', async () => {
    vi.mocked(ipc.listSessions).mockResolvedValue([
      rawSession('a', 'Older', '/a', '2024-01-01T00:00:00Z'),
      rawSession('b', 'Newer', '/b', '2024-06-01T00:00:00Z'),
    ]);

    await useSessionStore.getState().refresh();

    const { sessions, loading } = useSessionStore.getState();
    expect(loading).toBe(false);
    expect(sessions.map((s) => s.sessionId)).toEqual(['b', 'a']);
  });

  it('resets loading to false even if listSessions rejects', async () => {
    vi.mocked(ipc.listSessions).mockRejectedValue(new Error('boom'));

    await expect(useSessionStore.getState().refresh()).rejects.toThrow('boom');
    expect(useSessionStore.getState().loading).toBe(false);
  });
});

describe('sessionStore.remove', () => {
  it('deletes via ipc with the session cwd and drops it from state', async () => {
    useSessionStore.setState({
      sessions: [
        { sessionId: 's1', title: 'A', cwd: '/dir', updatedAt: 't' },
        { sessionId: 's2', title: 'B', cwd: '/other', updatedAt: 't' },
      ],
    });

    await useSessionStore.getState().remove('s1');

    expect(ipc.deleteSession).toHaveBeenCalledWith('s1', '/dir');
    expect(useSessionStore.getState().sessions.map((s) => s.sessionId)).toEqual(['s2']);
  });

  it('clears a dangling folder assignment for the removed session', async () => {
    useSessionStore.setState({
      sessions: [{ sessionId: 's1', title: 'A', cwd: '/dir', updatedAt: 't' }],
      assignments: { s1: 'Work' },
    });

    await useSessionStore.getState().remove('s1');

    expect(ipc.assignSessionFolder).toHaveBeenCalledWith('s1', null);
  });

  it('does not touch folder assignment when the session had none', async () => {
    useSessionStore.setState({
      sessions: [{ sessionId: 's1', title: 'A', cwd: '/dir', updatedAt: 't' }],
      assignments: {},
    });

    await useSessionStore.getState().remove('s1');

    expect(ipc.assignSessionFolder).not.toHaveBeenCalled();
  });
});

describe('sessionStore.rename', () => {
  it('renames via ipc and updates the local title', async () => {
    useSessionStore.setState({
      sessions: [{ sessionId: 's1', title: 'Old', cwd: '/dir', updatedAt: 't' }],
    });

    await useSessionStore.getState().rename('s1', '  New Title  ');

    expect(ipc.renameSession).toHaveBeenCalledWith('s1', 'New Title');
    expect(useSessionStore.getState().sessions[0].title).toBe('New Title');
  });

  it('ignores a blank title without calling ipc', async () => {
    useSessionStore.setState({
      sessions: [{ sessionId: 's1', title: 'Old', cwd: '/dir', updatedAt: 't' }],
    });

    await useSessionStore.getState().rename('s1', '   ');

    expect(ipc.renameSession).not.toHaveBeenCalled();
    expect(useSessionStore.getState().sessions[0].title).toBe('Old');
  });
});

describe('sessionStore.filtered', () => {
  it('returns everything when the query is empty', () => {
    useSessionStore.setState({
      sessions: [{ sessionId: 's1', title: 'Anything', cwd: '/x', updatedAt: 't' }],
      query: '',
    });
    expect(useSessionStore.getState().filtered()).toHaveLength(1);
  });

  it('matches on title or cwd, case-insensitively', () => {
    useSessionStore.setState({
      sessions: [
        { sessionId: 's1', title: 'Fix the bug', cwd: '/repo', updatedAt: 't' },
        { sessionId: 's2', title: 'Unrelated', cwd: '/other', updatedAt: 't' },
      ],
      query: 'BUG',
    });
    expect(
      useSessionStore
        .getState()
        .filtered()
        .map((s) => s.sessionId)
    ).toEqual(['s1']);
  });
});

describe('sessionStore folder operations', () => {
  it('createFolder calls ipc then refreshes folder state', async () => {
    vi.mocked(ipc.listFolders).mockResolvedValue({
      folders: ['Work'],
      assignments: {},
    });

    await useSessionStore.getState().createFolder('Work');

    expect(ipc.createFolder).toHaveBeenCalledWith('Work');
    expect(useSessionStore.getState().folders).toEqual(['Work']);
  });

  it('refreshFolders leaves existing state alone on failure', async () => {
    useSessionStore.setState({ folders: ['Existing'], assignments: { s1: 'Existing' } });
    vi.mocked(ipc.listFolders).mockRejectedValue(new Error('offline'));

    await useSessionStore.getState().refreshFolders();

    expect(useSessionStore.getState().folders).toEqual(['Existing']);
  });
});

describe('sessionStore.grouped', () => {
  it('buckets sessions by folder assignment and collects the rest under Uncategorized', () => {
    useSessionStore.setState({
      sessions: [
        { sessionId: 's1', title: 'A', cwd: '/a', updatedAt: 't' },
        { sessionId: 's2', title: 'B', cwd: '/b', updatedAt: 't' },
        { sessionId: 's3', title: 'C', cwd: '/c', updatedAt: 't' },
      ],
      folders: ['Work'],
      assignments: { s1: 'Work' },
      query: '',
    });

    const groups = useSessionStore.getState().grouped();

    const work = groups.find((g) => g.folder === 'Work');
    const uncategorized = groups.find((g) => g.folder === UNCATEGORIZED);
    expect(work?.sessions.map((s) => s.sessionId)).toEqual(['s1']);
    expect(uncategorized?.sessions.map((s) => s.sessionId).sort()).toEqual(['s2', 's3']);
  });

  it('treats a stale assignment pointing at a deleted folder as Uncategorized', () => {
    useSessionStore.setState({
      sessions: [{ sessionId: 's1', title: 'A', cwd: '/a', updatedAt: 't' }],
      folders: [],
      assignments: { s1: 'DeletedFolder' },
      query: '',
    });

    const groups = useSessionStore.getState().grouped();
    const uncategorized = groups.find((g) => g.folder === UNCATEGORIZED);
    expect(uncategorized?.sessions.map((s) => s.sessionId)).toEqual(['s1']);
  });
});
