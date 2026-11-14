// Inline figures: parsing, sanitizing and mounting the output of kitty-tools'
// visualization tools (`plugins/kitty-tools/src/tools/viz`).
//
// A figure is a fragment of HTML/SVG. It is sanitized here and mounted in a
// shadow root on an element of the chat itself, with `themes/viz.css` adopted
// into that root. Compared with the sandboxed iframe this replaced:
//
//   - there is no frame to size, so no height handshake to lose (the frames
//     that came out too short, or with no height at all);
//   - the theme's custom properties inherit into the shadow tree, so a figure
//     follows the app's theme, including a user's own;
//   - the shadow boundary keeps the figure's ids and styles (Mermaid ships a
//     stylesheet inside its SVG) from meeting the app's, in either direction.
//
// What the iframe gave for free and this has to do itself is isolation from
// the markup's *behaviour*: the fragment goes through DOMPurify with every
// scripting, linking, embedding and form element forbidden, and what CSS
// survives is checked for external references. The app's CSP
// (`default-src 'self'`) is the backstop for both.

import DOMPurify from 'dompurify';
import vizCss from '@/themes/viz.css?raw';

export type VizKind = 'chart' | 'diagram' | 'table' | 'mermaid';

export interface VizPayload {
  title?: string;
  kind?: VizKind;
  /** `html_payload`, unsanitized: a `<figure>` fragment from a current
      kitty-tools, or a whole HTML document from one that predates inline
      rendering (old chats keep their tool results). */
  html: string;
}

const KINDS: readonly string[] = ['chart', 'diagram', 'table', 'mermaid'];

/** Recognize a visualization tool result: the JSON envelope built by
    `success_payload` in `viz/mod.rs`. `target` is `inline` today and was
    `iframe` before; both carry markup in `html_payload` and are rendered the
    same way. Anything else — an error envelope, a truncated string, another
    tool's output — is `null`, and the card shows it as text. */
export function parseVizPayload(output: unknown): VizPayload | null {
  if (typeof output !== 'string') return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(output);
  } catch {
    return null;
  }
  if (typeof parsed !== 'object' || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  const renderConfig = obj.render_config;
  if (typeof renderConfig !== 'object' || renderConfig === null) return null;
  const rc = renderConfig as Record<string, unknown>;
  if (rc.target !== 'inline' && rc.target !== 'iframe') return null;
  const html = obj.html_payload;
  if (typeof html !== 'string' || !html.trim()) return null;
  return {
    title: typeof rc.title === 'string' ? rc.title : undefined,
    kind: typeof rc.kind === 'string' && KINDS.includes(rc.kind) ? (rc.kind as VizKind) : undefined,
    html,
  };
}

const FORBID_TAGS = [
  'script',
  'foreignObject',
  'iframe',
  'object',
  'embed',
  'a',
  'img',
  'image',
  'use',
  'link',
  'meta',
  'base',
  'form',
  'input',
  'button',
  'select',
  'textarea',
  'video',
  'audio',
  'source',
  'animate',
  'animateMotion',
  'animateTransform',
  'set',
];
const FORBID_ATTR = ['href', 'xlink:href', 'src', 'srcset', 'action', 'formaction', 'ping'];

/** True for CSS that reaches outside the figure: an `@import`, or a `url()`
    that is not a reference to the fragment's own `<defs>`. */
export function cssReachesOut(css: string): boolean {
  return /@import|expression\s*\(|url\(\s*['"]?\s*(?!#)/i.test(css);
}

function viewBoxSize(svg: Element): [number, number] | null {
  const parts = (svg.getAttribute('viewBox') ?? '')
    .trim()
    .split(/[\s,]+/)
    .map(Number);
  if (parts.length === 4 && parts.every(Number.isFinite) && parts[2] > 0 && parts[3] > 0) {
    return [parts[2], parts[3]];
  }
  const w = Number.parseFloat(svg.getAttribute('width') ?? '');
  const h = Number.parseFloat(svg.getAttribute('height') ?? '');
  return w > 0 && h > 0 && !/%/.test(svg.getAttribute('width') ?? '') ? [w, h] : null;
}

/** Smallest scale a figure is shrunk to before the card scrolls sideways
    instead — below this its 11–13px type stops being readable. */
const MIN_SCALE = 0.7;

/** Give a root `<svg>` a size that cannot collapse and cannot blow up: its
    natural width, scaled down (never up) to fit the column, with the height
    following from the aspect ratio — both read off the `viewBox`, nothing
    measured. Upscaling was half of why the old output looked wrong: a
    three-bar chart was stretched to the card's width with its type along
    for the ride. */
function sizeSvg(svg: SVGElement) {
  const size = viewBoxSize(svg);
  if (!size) return;
  const [w, h] = size;
  svg.removeAttribute('width');
  svg.removeAttribute('height');
  svg.style.width = `max(min(100%, ${w}px), ${Math.round(w * MIN_SCALE)}px)`;
  svg.style.height = 'auto';
  svg.style.maxWidth = 'none';
  svg.style.aspectRatio = `${w} / ${h}`;
  svg.style.backgroundColor = '';
}

/** Sanitize a figure's markup into a fragment that is safe to mount in the
    app's own document, and normalize it for the stylesheet. */
export function sanitizeViz(html: string): DocumentFragment {
  const fragment = DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true, svg: true, svgFilters: false },
    FORBID_TAGS,
    FORBID_ATTR,
    RETURN_DOM_FRAGMENT: true,
  }) as DocumentFragment | null;
  // An engine DOMPurify cannot run in yields nothing; show nothing rather
  // than fall back to the raw markup.
  if (!fragment || !DOMPurify.isSupported) return document.createDocumentFragment();

  // Only Mermaid ships a stylesheet of its own that the figure needs (its
  // palette is rewritten to the theme's roles server-side). Any other
  // `<style>` is either the baked palette of a pre-inline figure or does not
  // belong; inline `style` attributes likewise.
  const isMermaid = fragment.querySelector('.viz-mermaid, svg#merman') !== null;
  for (const el of Array.from(fragment.querySelectorAll('style'))) {
    if (!isMermaid || cssReachesOut(el.textContent ?? '')) el.remove();
  }
  for (const el of Array.from(fragment.querySelectorAll('[style]'))) {
    if (!isMermaid || cssReachesOut(el.getAttribute('style') ?? '')) el.removeAttribute('style');
  }

  const isCurrent = fragment.querySelector('figure.viz') !== null;
  for (const svg of Array.from(fragment.querySelectorAll('svg'))) {
    if (svg.parentElement?.closest('svg')) continue;
    if (!isCurrent && !isMermaid) svg.classList.add('legacy');
    sizeSvg(svg as SVGElement);
  }
  return fragment;
}

let sheet: CSSStyleSheet | null | undefined;

/** One constructed stylesheet shared by every figure's shadow root; `null`
    where the engine has no constructable stylesheets. */
function sharedSheet(): CSSStyleSheet | null {
  if (sheet !== undefined) return sheet;
  try {
    sheet = new CSSStyleSheet();
    sheet.replaceSync(vizCss);
  } catch {
    sheet = null;
  }
  return sheet;
}

/** Mount a figure in `host`'s shadow root, replacing whatever was there. */
export function mountViz(host: HTMLElement, html: string): ShadowRoot {
  const root = host.shadowRoot ?? host.attachShadow({ mode: 'open' });
  const fragment = sanitizeViz(html);
  const shared = sharedSheet();
  if (shared && 'adoptedStyleSheets' in root) {
    root.adoptedStyleSheets = [shared];
    root.replaceChildren(fragment);
  } else {
    const style = document.createElement('style');
    style.textContent = vizCss;
    root.replaceChildren(style, fragment);
  }
  return root;
}

const ROLES = ['ink', 'paper', 'rule', 'muted', 'tint', 'accent', 'font'];

const escapeHtml = (s: string) =>
  s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c] ?? c);

/** A self-contained HTML document of a mounted figure, for the card's
    Download: the sanitized markup, the stylesheet, and the theme's roles
    resolved to the values in effect now (a saved file has no theme to inherit
    them from). */
export function standaloneVizDocument(host: HTMLElement, title: string): string {
  const root = host.shadowRoot;
  if (!root) return '';
  // The stylesheet resolves each role from its public `--viz-*` property, so
  // the resolved values are handed over under those names.
  const computed = getComputedStyle(host);
  const roles = ROLES.map((r) => [r, computed.getPropertyValue(`--vz-${r}`).trim()] as const)
    .filter(([, v]) => v && !/[<>{}]/.test(v))
    .map(([r, v]) => `--viz-${r}:${v};`)
    .join('');
  const markup = Array.from(root.children)
    .filter((el) => el.tagName !== 'STYLE')
    .map((el) => el.outerHTML)
    .join('');
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${escapeHtml(title)}</title>
<style>body{margin:0;padding:32px;${roles}background:var(--viz-paper);}</style>
<style>${vizCss.replace(/<\//g, '<\\/')}</style>
</head>
<body><div class="viz-root">${markup}</div></body>
</html>`;
}
