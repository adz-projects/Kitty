// @vitest-environment node
// Every built-in theme must keep its text pairs at WCAG AA, and every
// built-in theme must define the same tokens — a token added to one file and
// forgotten in another would silently fall back.
// @ts-expect-error -- Vitest runs in Node; Kitty's tsconfig omits @types/node for browser safety.
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const read = (name: string): string =>
  (readFileSync as (p: URL, enc: string) => string)(new URL(`./${name}`, import.meta.url), 'utf8');
const THEMES: Record<string, string> = {
  'classic dark': read('dark.css'),
  'classic light': read('light.css'),
  'modern dark': read('modern-dark.css'),
  'modern light': read('modern-light.css'),
  'cli dark': read('cli-dark.css'),
  'cli light': read('cli-light.css'),
};

function tokens(css: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of css.matchAll(/--([\w-]+):\s*([^;]+);/g)) {
    out[m[1]] = m[2].trim();
  }
  return out;
}

type RGB = [number, number, number];

function hex(v: string): RGB | null {
  const m = /^#([0-9a-f]{6})$/i.exec(v);
  if (!m) return null;
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function rgba(v: string): { rgb: RGB; a: number } | null {
  const m = /^rgba\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*,\s*([\d.]+)\s*\)$/.exec(v);
  if (!m) return null;
  return { rgb: [+m[1], +m[2], +m[3]], a: +m[4] };
}

/** A translucent colour as it lands over a base. */
function over(v: string, base: RGB): RGB {
  const c = rgba(v);
  if (!c) {
    const solid = hex(v);
    if (!solid) throw new Error(`not a colour: ${v}`);
    return solid;
  }
  return c.rgb.map((ch, i) => Math.round(c.a * ch + (1 - c.a) * base[i])) as RGB;
}

function luminance([r, g, b]: RGB): number {
  const f = (v: number) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
}

function ratio(a: RGB, b: RGB): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

describe('theme tokens', () => {
  it('every theme defines the same tokens', () => {
    const sets = Object.entries(THEMES).map(
      ([name, css]) => [name, Object.keys(tokens(css))] as const
    );
    const reference = sets[0][1].slice().sort();
    for (const [name, keys] of sets) {
      expect(keys.slice().sort(), name).toEqual(reference);
    }
  });

  it('the modern styles set chat reading text in Open Sans', () => {
    for (const name of ['modern dark', 'modern light']) {
      expect(tokens(THEMES[name])['font-reading']).toContain('Open Sans');
    }
  });

  it('the cli styles set everything in monospace with sharp corners', () => {
    for (const name of ['cli dark', 'cli light']) {
      const t = tokens(THEMES[name]);
      expect(t['font-reading']).toContain('monospace');
      expect(t['font-family']).toContain('monospace');
      expect(t.radius).toBe('0px');
      expect(t['radius-sm']).toBe('0px');
    }
  });

  for (const [name, css] of Object.entries(THEMES)) {
    it(`${name}: text pairs meet WCAG AA`, () => {
      const t = tokens(css);
      const solid = (key: string): RGB => {
        const c = hex(t[key]);
        if (!c) throw new Error(`${name}: --${key} is not a solid colour`);
        return c;
      };
      const bg = solid('bg');
      const surface = solid('surface');
      const surface2 = solid('surface-2');
      const pairs: [string, RGB, RGB][] = [
        ['text on bg', solid('text'), bg],
        ['text on surface', solid('text'), surface],
        ['text on surface-2', solid('text'), surface2],
        ['muted on bg', solid('text-muted'), bg],
        ['muted on surface', solid('text-muted'), surface],
        ['chat-text on bg', solid('chat-text'), bg],
        ['chat-text on surface-2 (user bubble)', solid('chat-text'), surface2],
        ['chat-strong on bg', solid('chat-strong'), bg],
        ['accent on bg', solid('accent'), bg],
        ['accent on surface', solid('accent'), surface],
        ['accent-text on accent-fill', solid('accent-text'), solid('accent-fill')],
        ['find-current-text on find-current', solid('find-current-text'), solid('find-current')],
        ['text on find-match', solid('text'), solid('find-match')],
        ['ok on bg', solid('ok'), bg],
        ['warn on bg', solid('warn'), bg],
        ['danger on bg', solid('danger'), bg],
        ['ok on ok-bg', solid('ok'), over(t['ok-bg'], surface)],
        ['warn on warn-bg', solid('warn'), over(t['warn-bg'], surface)],
        ['danger on danger-bg', solid('danger'), over(t['danger-bg'], surface)],
      ];
      for (const [label, a, b] of pairs) {
        expect(ratio(a, b), `${name}: ${label}`).toBeGreaterThanOrEqual(4.5);
      }
    });
  }
});
