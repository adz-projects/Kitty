// Runtime theming (Phase 6). The active theme's custom properties are injected
// as a <style> tag so switching (built-in or user CSS) needs no rebuild. base.css
// (structural) stays statically imported. Background image is applied as a data
// URL on the document root. See themes/README.md for the theme contract.

import defaultCss from '@/themes/default.css?raw';
import darkCss from '@/themes/dark.css?raw';
import { ipc, onThemeChanged } from './ipc';
import type { Config } from './types';

const BUILTIN: Record<string, string> = { default: defaultCss, dark: darkCss };

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
    // Unknown/missing theme (renamed/deleted user file) — default is the
    // only sane content, but log so the silent swap isn't invisible.
    console.warn(`theme "${name}" could not be read; falling back to default`, e);
    return BUILTIN.default;
  }
}

// Monotonic apply-generation guard: `applyFromConfig` is fully async (two IPC
// round-trips), and rapid theme://changed events can overlap — the OLDER
// request may resolve LAST and clobber the newer config onto the DOM. Each
// call stamps itself; a result whose generation is no longer current is
// dropped instead of applied.
let themeApplyGen = 0;

// Last image path we already re-encoded, so a pure THEME switch (same
// wallpaper configured) doesn't re-read + re-base64 the whole file.
let lastBgImagePath: string | null = null;
let lastBgImageUrl: string | null = null;

/** Windows' own wallpaper-fit terms (Fill/Fit/Stretch/Center), mapped to CSS
    `background-size`/`background-repeat` (Round-4 item 2). "Center" (actual
    size, no scaling) needs `auto` + `no-repeat` since `background-size` alone
    would otherwise default to tiling at natural size. */
const BG_SIZE_CSS: Record<Config['background_size'], string> = {
  cover: 'cover',
  contain: 'contain',
  stretch: '100% 100%',
  center: 'auto',
};

async function applyBackground(cfg: Config) {
  const root = document.documentElement;
  root.style.setProperty('--bg-image-dim', String(cfg.background_dim ?? 0.3));
  root.style.setProperty(
    '--bg-position',
    `${cfg.background_position_x ?? 50}% ${cfg.background_position_y ?? 50}%`
  );
  root.style.setProperty('--bg-size', BG_SIZE_CSS[cfg.background_size] ?? 'cover');
  if (cfg.background_image) {
    // Skip the full-file re-read when only the theme changed (same wallpaper
    // still configured) — the data URL is unchanged, so a redundant IPC
    // round-trip + base64 encode per theme://changed event is pure waste.
    const cachedUrl = lastBgImagePath === cfg.background_image ? lastBgImageUrl : null;
    if (cachedUrl) {
      root.style.setProperty('--bg-image', `url("${cachedUrl}")`);
      root.setAttribute('data-bg-image', '');
      return;
    }
    try {
      const url = await ipc.readImageDataUrl(cfg.background_image);
      lastBgImagePath = cfg.background_image;
      lastBgImageUrl = url;
      root.style.setProperty('--bg-image', `url("${url}")`);
      root.setAttribute('data-bg-image', '');
      return;
    } catch {
      /* fall through to clear */
    }
  }
  root.style.removeProperty('--bg-image');
  root.removeAttribute('data-bg-image');
  // Wallpaper was cleared — drop the cache so a later re-set re-reads.
  lastBgImagePath = null;
  lastBgImageUrl = null;
}

async function applyFromConfig() {
  const gen = ++themeApplyGen;
  const cfg = await ipc.getConfig();
  // A newer applyFromConfig() started while we were waiting — drop this
  // stale result so an older config can't clobber the newer one on the DOM.
  if (gen !== themeApplyGen) return;
  ensureStyle('app-theme').textContent = await themeCss(cfg.theme);
  await applyBackground(cfg);
}

/** Apply the configured theme/background and keep it in sync with changes. */
export function initTheme() {
  // Inject the default synchronously to avoid an unstyled flash, then apply
  // the real configured theme.
  ensureStyle('app-theme').textContent = BUILTIN.default;
  void applyFromConfig();
  void onThemeChanged(() => void applyFromConfig());
}
