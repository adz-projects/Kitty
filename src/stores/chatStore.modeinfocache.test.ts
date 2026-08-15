import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { readCachedModeInfo, writeCachedModeInfo, modeInfoCacheKey } from './chatStore';

/** Backs the fix for the effort/mode dropdown delay on a *brand-new* window's
    first-ever "New Chat": `newSession()`'s existing same-window carry-forward
    only helps the 2nd+ session in a window's lifetime, since there's nothing
    to carry forward before any session has ever been created there. This
    localStorage cache (shared across windows, same webview origin) covers
    that gap by seeding the last-known values for the active provider. */

/** A real, minimal `Storage` — not a mock with `vi.fn()` stubs.
 *
 * This file used to ask for the `happy-dom` environment via a `//
 * @vitest-environment` comment, which was wrong twice over: Vitest only reads
 * that pragma from a docblock, and happy-dom 15 doesn't expose `localStorage`
 * under this Node anyway (neither as a global nor on `window` — verified).
 * The result was five tests dying in `beforeEach` on `undefined.clear()`.
 *
 * These tests want a Storage, not a DOM, so providing one directly removes
 * the environment from the equation entirely and keeps the assertions real:
 * the actual `JSON.parse`/`JSON.stringify` round trip still runs, rather than
 * `readCachedModeInfo`'s `catch` swallowing a missing API and returning
 * `null` — which would have made every test here pass while proving nothing. */
function memoryStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (k: string) => map.get(k) ?? null,
    key: (i: number) => [...map.keys()][i] ?? null,
    removeItem: (k: string) => void map.delete(k),
    setItem: (k: string, v: string) => void map.set(k, String(v)),
  };
}

const hadLocalStorage = 'localStorage' in globalThis;

describe('mode info cache', () => {
  beforeEach(() => {
    Object.defineProperty(globalThis, 'localStorage', {
      value: memoryStorage(),
      configurable: true,
      writable: true,
    });
  });

  afterEach(() => {
    if (!hadLocalStorage) {
      // Leave the global exactly as found, so a later test file that expects
      // no `localStorage` isn't quietly handed ours.
      delete (globalThis as { localStorage?: Storage }).localStorage;
    }
  });

  it('round-trips mode/availableModes for a provider id', () => {
    const info = {
      mode: 'auto',
      availableModes: [{ id: 'auto', name: 'Auto', description: 'test' }],
    };
    writeCachedModeInfo('provider-1', info);
    expect(readCachedModeInfo('provider-1')).toEqual(info);
  });

  it('returns null when nothing has been cached for a provider id', () => {
    expect(readCachedModeInfo('never-seen')).toBeNull();
  });

  it('keys entries per provider id — does not leak across providers', () => {
    writeCachedModeInfo('provider-a', { mode: 'auto', availableModes: [] });
    writeCachedModeInfo('provider-b', { mode: 'manual', availableModes: [] });
    expect(readCachedModeInfo('provider-a')?.mode).toBe('auto');
    expect(readCachedModeInfo('provider-b')?.mode).toBe('manual');
  });

  it('returns null (not a throw) for corrupted JSON in storage', () => {
    localStorage.setItem(modeInfoCacheKey('bad'), 'not json{{');
    expect(readCachedModeInfo('bad')).toBeNull();
  });

  it('a later write for the same provider id overwrites the earlier one', () => {
    writeCachedModeInfo('provider-1', { mode: 'auto', availableModes: [] });
    writeCachedModeInfo('provider-1', { mode: 'manual', availableModes: [] });
    expect(readCachedModeInfo('provider-1')?.mode).toBe('manual');
  });

  /** The write path swallows failures by design (a full or unavailable
      Storage just means no seed next time). Pin that it degrades rather than
      throwing into the caller — `newSession` calls this on a hot path. */
  it('a failing storage degrades quietly instead of throwing', () => {
    Object.defineProperty(globalThis, 'localStorage', {
      value: {
        ...memoryStorage(),
        setItem: () => {
          throw new Error('QuotaExceededError');
        },
      },
      configurable: true,
      writable: true,
    });
    expect(() =>
      writeCachedModeInfo('provider-1', { mode: 'auto', availableModes: [] })
    ).not.toThrow();
    expect(readCachedModeInfo('provider-1')).toBeNull();
  });
});
