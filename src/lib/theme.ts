// Runtime theming (Phase 6). The active theme's custom properties are injected
// as a <style> tag so switching (built-in or user CSS) needs no rebuild. base.css
// (structural) stays statically imported. See themes/README.md for the theme
// contract.

import lightCss from '@/themes/light.css?raw';
import darkCss from '@/themes/dark.css?raw';
import { ipc, onThemeChanged } from './ipc';
import { applyPlatformAttribute } from './platform';

const BUILTIN: Record<string, string> = { light: lightCss, dark: darkCss };

/** `theme: "system"` follows the OS (D16): `light` under
    `prefers-color-scheme: light`, `dark` under dark. Not a third stylesheet —
    it resolves to one of the two built-ins, so a user theme keeps behaving
    exactly as before and only this one reserved name is special. */
export const SYSTEM_THEME = 'system';

/** Resolve a configured theme name against an OS colour-scheme preference.
    Pure, so the mapping is testable without a media-query environment.
    `"default"` is also accepted as an alias of `"light"` — the built-in's old
    id, for a config/DOM value that hasn't gone through the Rust-side
    migration yet (e.g. mid-session before a reload). */
export function resolveThemeName(configured: string, prefersDark: boolean): string {
  if (configured === SYSTEM_THEME) return prefersDark ? 'dark' : 'light';
  return configured === 'default' ? 'light' : configured;
}

function prefersDark(): boolean {
  return (
    typeof window !== 'undefined' &&
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(prefers-color-scheme: dark)').matches
  );
}

function ensureStyle(id: string): HTMLStyleElement {
  let el = document.getElementById(id) as HTMLStyleElement | null;
  if (!el) {
    el = document.createElement('style');
    el.id = id;
    document.head.appendChild(el);
  }
  return el;
}

// Last-successfully-loaded user theme CSS, keyed by theme name — a transient
// read failure on a *valid* theme name falls back to the previously-loaded
// content for that name instead of silently wrenching the user to `default`.
const userThemeCache = new Map<string, string>();

async function themeCss(name: string): Promise<string> {
  if (BUILTIN[name]) return BUILTIN[name];
  try {
    const css = await ipc.readUserTheme(name);
    userThemeCache.set(name, css);
    return css;
  } catch (e) {
    const cached = userThemeCache.get(name);
    if (cached !== undefined) return cached;
    // Unknown/missing theme (renamed/deleted user file) — light is the
    // only sane content, but log so the silent swap isn't invisible.
    console.warn(`theme "${name}" could not be read; falling back to light`, e);
    return BUILTIN.light;
  }
}

// Monotonic apply-generation guard: `applyFromConfig` is fully async (an IPC
// round-trip), and rapid theme://changed events can overlap — the OLDER
// request may resolve LAST and clobber the newer config onto the DOM. Each
// call stamps itself; a result whose generation is no longer current is
// dropped instead of applied.
let themeApplyGen = 0;

async function applyFromConfig() {
  const gen = ++themeApplyGen;
  try {
    const cfg = await ipc.getConfig();
    // A newer applyFromConfig() started while we were waiting — drop this
    // stale result so an older config can't clobber the newer one on the DOM.
    if (gen !== themeApplyGen) return;
    const css = await themeCss(resolveThemeName(cfg.theme, prefersDark()));
    // themeCss may itself await an IPC round-trip (readUserTheme) — re-check
    // before writing, otherwise a slow stale call can still win the DOM write.
    if (gen !== themeApplyGen) return;
    ensureStyle('app-theme').textContent = css;
  } catch {
    // Backend down / IPC failure — keep whatever theme is already applied
    // (initTheme's synchronously-injected built-in). Caught here, once, so
    // the `void applyFromConfig()` call sites can't produce an unhandled
    // rejection; the next theme://changed event retries anyway.
  }
}

/** Apply the configured theme/background and keep it in sync with changes. */
export function initTheme() {
  // Stamp the platform before anything paints — safe-area insets and the
  // mobile shell are gated on it, and doing this after an await would flash
  // the desktop layout on a phone.
  applyPlatformAttribute();
  // Inject a synchronously-resolved built-in to avoid an unstyled flash. Uses
  // the OS preference rather than always `light`, so a dark-mode user on
  // `theme: "system"` doesn't get a white flash before the config loads.
  ensureStyle('app-theme').textContent = prefersDark() ? BUILTIN.dark : BUILTIN.light;
  void applyFromConfig();
  void onThemeChanged(() => void applyFromConfig());
  // Follow the OS while `theme: "system"` is active. `applyFromConfig`
  // re-reads the config and re-resolves, so this is a no-op for a user on a
  // pinned theme — no need to check which mode is set here.
  if (typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
    window
      .matchMedia('(prefers-color-scheme: dark)')
      .addEventListener('change', () => void applyFromConfig());
  }
}
