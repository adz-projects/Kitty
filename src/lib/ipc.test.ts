import { describe, it, expect, vi, beforeEach } from 'vitest';

// `ipc.ts` is the sole `invoke()`/`listen()` chokepoint (CLAUDE.md rule 2) —
// these tests mock both Tauri entry points and assert each wrapper calls the
// right command name with the right argument shape, since a typo there is
// invisible to `tsc` (the command name is just a string) and would otherwise
// only surface as a runtime "unknown command" failure inside the real app.
const invokeMock = vi.fn().mockResolvedValue(undefined);
const listenMock = vi.fn().mockResolvedValue(() => {});

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (...args: unknown[]) => listenMock(...args),
}));
vi.mock('@tauri-apps/api/webview', () => ({
  getCurrentWebview: () => ({ onDragDropEvent: vi.fn() }),
}));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  open: vi.fn(),
  save: vi.fn(),
}));

const {
  ipc,
  onStackStatus,
  onAdaptivePathwayEmbeddingStatus,
  onMessageDelta,
  onModelProgress,
  onModelsChanged,
  onEngineRestartState,
} = await import('./ipc');

beforeEach(() => {
  invokeMock.mockClear();
  listenMock.mockClear();
});

describe('ipc invoke wrappers', () => {
  it('getConfig calls get_config with no args', () => {
    void ipc.getConfig();
    expect(invokeMock).toHaveBeenCalledWith('get_config');
  });

  it('setConfig calls set_config with the config payload', () => {
    const config = { adaptive_pathway_enabled: true } as unknown as Parameters<
      typeof ipc.setConfig
    >[0];
    void ipc.setConfig(config);
    expect(invokeMock).toHaveBeenCalledWith('set_config', { config });
  });

  it('toggleOverlay calls toggle_overlay with no args', () => {
    void ipc.toggleOverlay();
    expect(invokeMock).toHaveBeenCalledWith('toggle_overlay');
  });

  it('openSettings normalizes omitted section/highlight to null', () => {
    void ipc.openSettings();
    expect(invokeMock).toHaveBeenCalledWith('open_settings', {
      section: null,
      highlight: null,
    });
  });

  it('openSettings passes through provided section/highlight', () => {
    void ipc.openSettings('providers', 'p1');
    expect(invokeMock).toHaveBeenCalledWith('open_settings', {
      section: 'providers',
      highlight: 'p1',
    });
  });

  it('loadSession calls load_session with sessionId and cwd', () => {
    void ipc.loadSession('s1', '/tmp');
    expect(invokeMock).toHaveBeenCalledWith('load_session', { sessionId: 's1', cwd: '/tmp' });
  });

  it('deleteSession normalizes an omitted cwd to null', () => {
    void ipc.deleteSession('s1');
    expect(invokeMock).toHaveBeenCalledWith('delete_session', { sessionId: 's1', cwd: null });
  });

  it('listProviders calls list_providers with no args', () => {
    void ipc.listProviders();
    expect(invokeMock).toHaveBeenCalledWith('list_providers');
  });

  it('activateProvider calls activate_provider with the id', () => {
    void ipc.activateProvider('p1');
    expect(invokeMock).toHaveBeenCalledWith('activate_provider', { id: 'p1', sessionId: null });
  });

  it('activateProvider allows a null id (deactivate)', () => {
    void ipc.activateProvider(null);
    expect(invokeMock).toHaveBeenCalledWith('activate_provider', { id: null, sessionId: null });
  });

  it('activateProvider forwards an optional session id for per-session stamping', () => {
    void ipc.activateProvider('p2', 'session-1');
    expect(invokeMock).toHaveBeenCalledWith('activate_provider', {
      id: 'p2',
      sessionId: 'session-1',
    });
  });

  it('setPathwaySessionPaused calls the right command with sessionId and paused', () => {
    void ipc.setPathwaySessionPaused('s1', true);
    expect(invokeMock).toHaveBeenCalledWith('set_pathway_session_paused', {
      sessionId: 's1',
      paused: true,
    });
  });

  it('getAdaptivePathwayMcpStatus calls get_adaptive_pathway_mcp_status with no args', () => {
    void ipc.getAdaptivePathwayMcpStatus();
    expect(invokeMock).toHaveBeenCalledWith('get_adaptive_pathway_mcp_status');
  });
});

describe('ipc event subscription wrappers', () => {
  it('onStackStatus subscribes to stack://status and unwraps the payload', async () => {
    const cb = vi.fn();
    await onStackStatus(cb);
    expect(listenMock).toHaveBeenCalledWith('stack://status', expect.any(Function));

    const handler = listenMock.mock.calls[0][1] as (e: { payload: unknown }) => void;
    handler({ payload: { status: 'ok' } });
    expect(cb).toHaveBeenCalledWith({ status: 'ok' });
  });

  it('onAdaptivePathwayEmbeddingStatus subscribes to the adaptive_pathway://embedding_status event', async () => {
    const cb = vi.fn();
    await onAdaptivePathwayEmbeddingStatus(cb);
    expect(listenMock).toHaveBeenCalledWith(
      'adaptive_pathway://embedding_status',
      expect.any(Function)
    );
  });

  it('listLocalModels calls list_local_models with no args', () => {
    void ipc.listLocalModels();
    expect(invokeMock).toHaveBeenCalledWith('list_local_models');
  });

  it('getLocalEngineStatus calls get_local_engine_status with no args', () => {
    void ipc.getLocalEngineStatus();
    expect(invokeMock).toHaveBeenCalledWith('get_local_engine_status');
  });

  it('getModelsDiskFree calls get_models_disk_free', () => {
    void ipc.getModelsDiskFree();
    expect(invokeMock).toHaveBeenCalledWith('get_models_disk_free');
  });

  it('deleteLocalModel passes the model id', () => {
    void ipc.deleteLocalModel('LFM2.5-1.2B-Instruct-Q4_K_M');
    expect(invokeMock).toHaveBeenCalledWith('delete_local_model', {
      id: 'LFM2.5-1.2B-Instruct-Q4_K_M',
    });
  });

  // The optional args must go over the wire as explicit `null`, not be
  // omitted: the Rust side takes `Option<String>`, and Tauri maps a missing
  // key to a deserialization error rather than `None`.
  it('downloadModel sends explicit nulls for the optional rev and id', () => {
    void ipc.downloadModel('acme/models', 'm.gguf');
    expect(invokeMock).toHaveBeenCalledWith('download_model', {
      repo: 'acme/models',
      file: 'm.gguf',
      rev: null,
      downloadId: null,
    });
  });

  it('downloadModel forwards a pre-agreed download id when given one', () => {
    void ipc.downloadModel('acme/models', 'm.gguf', 'main', 'fixed-id');
    expect(invokeMock).toHaveBeenCalledWith('download_model', {
      repo: 'acme/models',
      file: 'm.gguf',
      rev: 'main',
      downloadId: 'fixed-id',
    });
  });

  // Positional args, so an inserted parameter silently shifts every later
  // one — `modelId` landing where `schedule` was would be a runtime type
  // error deep in Rust, not a compile error here.
  it('createScheduledTask passes modelId in the right slot', () => {
    void ipc.createScheduledTask(
      'Nightly',
      'summarise',
      '/tmp',
      'LFM2.5-1.2B-Instruct-Q4_K_M',
      { kind: 'one_shot' },
      '2026-01-01T00:00:00Z'
    );
    expect(invokeMock).toHaveBeenCalledWith('create_scheduled_task', {
      name: 'Nightly',
      prompt: 'summarise',
      cwd: '/tmp',
      modelId: 'LFM2.5-1.2B-Instruct-Q4_K_M',
      schedule: { kind: 'one_shot' },
      nextFire: '2026-01-01T00:00:00Z',
    });
  });

  it('updateScheduledTask passes a null modelId as "no override"', () => {
    void ipc.updateScheduledTask(
      't1',
      'Nightly',
      'summarise',
      null,
      null,
      { kind: 'recurring', interval_secs: 3600 },
      '2026-01-01T00:00:00Z',
      true
    );
    expect(invokeMock).toHaveBeenCalledWith('update_scheduled_task', {
      id: 't1',
      name: 'Nightly',
      prompt: 'summarise',
      cwd: null,
      modelId: null,
      schedule: { kind: 'recurring', interval_secs: 3600 },
      nextFire: '2026-01-01T00:00:00Z',
      enabled: true,
    });
  });

  it('getEngineRestartState calls get_engine_restart_state', () => {
    void ipc.getEngineRestartState();
    expect(invokeMock).toHaveBeenCalledWith('get_engine_restart_state');
  });

  it('onMessageDelta subscribes to chat://message-delta and unwraps the payload', async () => {
    const cb = vi.fn();
    await onMessageDelta(cb);
    expect(listenMock).toHaveBeenCalledWith('chat://message-delta', expect.any(Function));

    const handler = listenMock.mock.calls[0][1] as (e: { payload: unknown }) => void;
    handler({ payload: { text: 'hi' } });
    expect(cb).toHaveBeenCalledWith({ text: 'hi' });
  });

  it('onModelProgress subscribes to models://progress and unwraps the payload', async () => {
    const cb = vi.fn();
    await onModelProgress(cb);
    expect(listenMock).toHaveBeenCalledWith('models://progress', expect.any(Function));

    const handler = listenMock.mock.calls[0][1] as (e: { payload: unknown }) => void;
    handler({ payload: { download_id: 'd1', model: 'm.gguf', received: 1, done: false } });
    expect(cb).toHaveBeenCalledWith({
      download_id: 'd1',
      model: 'm.gguf',
      received: 1,
      done: false,
    });
  });

  it('onModelsChanged subscribes to models://changed', async () => {
    const cb = vi.fn();
    await onModelsChanged(cb);
    expect(listenMock).toHaveBeenCalledWith('models://changed', expect.any(Function));
  });

  it('onEngineRestartState subscribes to engine://restart-state', async () => {
    const cb = vi.fn();
    await onEngineRestartState(cb);
    expect(listenMock).toHaveBeenCalledWith('engine://restart-state', expect.any(Function));
  });
});
