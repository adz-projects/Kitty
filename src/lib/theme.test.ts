// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest';

// Regression (88bugs #21): `applyFromConfig` is fully async across three
// separate IPC round-trips (getConfig, themeCss's readUserTheme, and
// applyBackground's readImageDataUrl). A stale call that started earlier but
// resolves later must never win the final DOM write over a newer call that
// started after it — the generation guard must be re-checked after EVERY
// await that precedes a DOM mutation, not just the first one.

const getConfig = vi.fn();
const readUserTheme = vi.fn();
const readImageDataUrl = vi.fn();
const onThemeChanged = vi.fn();

vi.mock('@/lib/ipc', () => ({
  ipc: { getConfig, readUserTheme, readImageDataUrl },
  onThemeChanged: (...args: unknown[]) => onThemeChanged(...args),
}));

vi.mock('@/themes/default.css?raw', () => ({ default: ':root{--default:1}' }));
vi.mock('@/themes/dark.css?raw', () => ({ default: ':root{--dark:1}' }));

function baseConfig(overrides: Record<string, unknown> = {}) {
  return {
    theme: 'default',
    background_image: null,
    background_dim: 0.3,
    background_position_x: 50,
    background_position_y: 50,
    background_size: 'cover',
    ...overrides,
  };
}

/** A promise plus its resolve function, for controlling await ordering. */
function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

beforeEach(() => {
  vi.resetModules();
  document.head.innerHTML = '';
  getConfig.mockReset();
  readUserTheme.mockReset();
  readImageDataUrl.mockReset();
  onThemeChanged.mockReset();
});

describe('theme.applyFromConfig', () => {
  it('does not let a slower, earlier-started call clobber a faster, later-started one at the readUserTheme stage', async () => {
    const { initTheme } = await import('./theme');

    const slowUser = deferred<string>();
    const fastUser = deferred<string>();
    // First call resolves getConfig immediately with a user theme "slow",
    // then stalls on readUserTheme. Second call resolves getConfig
    // immediately with user theme "fast" and its readUserTheme resolves
    // first.
    getConfig
      .mockResolvedValueOnce(baseConfig({ theme: 'slow' }))
      .mockResolvedValueOnce(baseConfig({ theme: 'fast' }));
    readUserTheme.mockImplementation((name: string) => {
      if (name === 'slow') return slowUser.promise;
      if (name === 'fast') return fastUser.promise;
      throw new Error(`unexpected theme ${name}`);
    });
    readImageDataUrl.mockResolvedValue('data:image/png;base64,');

    initTheme(); // call #1 ("slow"), fires and awaits getConfig then stalls on readUserTheme
    await Promise.resolve(); // let call #1 reach its readUserTheme await
    await Promise.resolve();

    // Reuse the same exported apply path via the theme://changed subscription
    // callback registered by initTheme, simulating a second, later config
    // change arriving while the first is still in flight.
    const onChangedCallback = onThemeChanged.mock.calls[0][0] as () => void;
    onChangedCallback(); // call #2 ("fast")
    await Promise.resolve();
    await Promise.resolve();

    // Let the faster call's readUserTheme resolve and fully apply first.
    fastUser.resolve('.fast{color:red}');
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    // Now the stale, slower call finally resolves. It must be dropped, not
    // overwrite the DOM with "slow"'s CSS.
    slowUser.resolve('.slow{color:blue}');
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    const styleEl = document.getElementById('app-theme');
    expect(styleEl?.textContent).toBe('.fast{color:red}');
  });
});
