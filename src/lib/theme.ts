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

async function themeCss(name: string): Promise<string> {
  if (BUILTIN[name]) return BUILTIN[name];
  try {
    return await ipc.readUserTheme(name);
  } catch {
    return BUILTIN.default;
  }
}

async function applyBackground(cfg: Config) {
  const root = document.documentElement;
  root.style.setProperty('--bg-image-dim', String(cfg.background_dim ?? 0.3));
  if (cfg.background_image) {
    try {
      const url = await ipc.readImageDataUrl(cfg.background_image);
      root.style.setProperty('--bg-image', `url("${url}")`);
      root.setAttribute('data-bg-image', '');
      return;
    } catch {
      /* fall through to clear */
    }
  }
  root.style.removeProperty('--bg-image');
  root.removeAttribute('data-bg-image');
}

async function applyFromConfig() {
  const cfg = await ipc.getConfig();
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
