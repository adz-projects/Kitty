// Runtime theming (Phase 6). The active theme's custom properties are injected
// as a <style> tag so switching (built-in or user CSS) needs no rebuild. base.css
// (structural) stays statically imported. See themes/README.md for the theme
// contract.

import lightCss from '@/themes/light.css?raw';
import darkCss from '@/themes/dark.css?raw';
import modernLightCss from '@/themes/modern-light.css?raw';
import modernDarkCss from '@/themes/modern-dark.css?raw';
import cliLightCss from '@/themes/cli-light.css?raw';
import cliDarkCss from '@/themes/cli-dark.css?raw';
import { ipc, onThemeChanged } from './ipc';
import { applyPlatformAttribute } from './platform';

export type ThemeStyle = 'classic' | 'modern' | 'cli';

const BUILTIN_STYLES: Record<ThemeStyle, { light: string; dark: string }> = {
  classic: { light: lightCss, dark: darkCss },
  modern: { light: modernLightCss, dark: modernDarkCss },
  cli: { light: cliLightCss, dark: cliDarkCss },
};

export function resolveThemeStyle(style?: string | null): ThemeStyle {
  if (style === 'modern') return 'modern';
  if (style === 'cli') return 'cli';
  return 'classic';
}

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

interface ResolvedTheme {
  css: string;
  dataTheme: 'light' | 'dark' | null;
  dataStyle: ThemeStyle | null;
}

function setRootThemeAttrs(dataTheme: 'light' | 'dark' | null, dataStyle: ThemeStyle | null) {
  const root = document.documentElement;
  if (dataTheme) root.setAttribute('data-theme', dataTheme);
  else root.removeAttribute('data-theme');
  if (dataStyle) root.setAttribute('data-theme-style', dataStyle);
  else root.removeAttribute('data-theme-style');
}

async function resolveThemePayload(name: string, style?: string | null): Promise<ResolvedTheme> {
  if (name === 'light' || name === 'dark') {
    const resolvedStyle = resolveThemeStyle(style);
    return {
      css: BUILTIN_STYLES[resolvedStyle][name],
      dataTheme: name,
      dataStyle: resolvedStyle,
    };
  }
  try {
    const css = await ipc.readUserTheme(name);
    userThemeCache.set(name, css);
    return { css, dataTheme: null, dataStyle: null };
  } catch (e) {
    const cached = userThemeCache.get(name);
    if (cached !== undefined) return { css: cached, dataTheme: null, dataStyle: null };
    // Unknown/missing theme (renamed/deleted user file) — light is the
    // only sane content, but log so the silent swap isn't invisible.
    console.warn(`theme "${name}" could not be read; falling back to light`, e);
    return { css: BUILTIN_STYLES.classic.light, dataTheme: 'light', dataStyle: 'classic' };
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
    const resolved = await resolveThemePayload(
      resolveThemeName(cfg.theme, prefersDark()),
      cfg.theme_style
    );
    // resolveThemePayload may itself await an IPC round-trip (readUserTheme) —
    // re-check before writing, otherwise a slow stale call can still win.
    if (gen !== themeApplyGen) return;
    ensureStyle('app-theme').textContent = resolved.css;
    setRootThemeAttrs(resolved.dataTheme, resolved.dataStyle);
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
  const initialMode = prefersDark() ? 'dark' : 'light';
  ensureStyle('app-theme').textContent = BUILTIN_STYLES.classic[initialMode];
  setRootThemeAttrs(initialMode, 'classic');
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
