// @vitest-environment jsdom
// jsdom, not the suite's default happy-dom: DOMPurify is a silent no-op under
// happy-dom (it returns its input unsanitized), so these tests would prove
// nothing there. jsdom is the DOM DOMPurify itself is tested against.
import { describe, expect, it } from 'vitest';
import { cssReachesOut, mountViz, parseVizPayload, sanitizeViz } from './viz';

const envelope = (target: string, html: string, extra: Record<string, unknown> = {}) =>
  JSON.stringify({
    status: 'success',
    render_config: { target, title: 'T', ...extra },
    html_payload: html,
  });

const html = (fragment: DocumentFragment) => {
  const div = document.createElement('div');
  div.append(fragment);
  return div.innerHTML;
};

describe('parseVizPayload', () => {
  it('reads the inline envelope with its kind', () => {
    const p = parseVizPayload(
      envelope('inline', '<figure class="viz"></figure>', { kind: 'chart' })
    );
    expect(p).toEqual({ title: 'T', kind: 'chart', html: '<figure class="viz"></figure>' });
  });

  it('still reads the envelope a pre-inline kitty-tools wrote', () => {
    const p = parseVizPayload(envelope('iframe', '<!DOCTYPE html><html></html>'));
    expect(p?.html).toContain('<!DOCTYPE html>');
    expect(p?.kind).toBeUndefined();
  });

  it('rejects everything that is not a figure', () => {
    expect(parseVizPayload(null)).toBeNull();
    expect(parseVizPayload('plain text')).toBeNull();
    expect(parseVizPayload(JSON.stringify({ status: 'error', message: 'x' }))).toBeNull();
    expect(parseVizPayload(envelope('popup', '<p>x</p>'))).toBeNull();
    expect(parseVizPayload(envelope('inline', '   '))).toBeNull();
    expect(
      parseVizPayload(envelope('inline', '<p>x</p>', { kind: 'nonsense' }))?.kind
    ).toBeUndefined();
  });
});

describe('cssReachesOut', () => {
  it('allows references to the fragment itself and plain declarations', () => {
    expect(cssReachesOut('.a{fill:url(#hatch-a);stroke:var(--vz-ink)}')).toBe(false);
    expect(cssReachesOut('max-width:316px;')).toBe(false);
  });

  it('flags anything fetched from elsewhere', () => {
    expect(cssReachesOut('@import "https://example.com/x.css";')).toBe(true);
    expect(cssReachesOut('.a{background:url(https://example.com/a.png)}')).toBe(true);
    expect(cssReachesOut(".a{background:url( 'data:image/png;base64,AAAA')}")).toBe(true);
  });
});

describe('sanitizeViz', () => {
  it('keeps a figure and sizes its svg from the viewBox, never upscaling', () => {
    const out = sanitizeViz(
      '<figure class="viz viz-chart"><figcaption class="viz-title">T</figcaption>' +
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 400 200" width="400" height="200" class="viz-svg">' +
        '<rect x="1" y="2" width="3" height="4" class="bar bar-2" fill="url(#hatch-a)"/>' +
        '<text x="1" y="2" class="cat-label-rot" transform="rotate(-45 1 2)">a</text></svg></figure>'
    );
    const svg = out.querySelector('svg') as SVGElement;
    expect(svg).not.toBeNull();
    expect(svg.hasAttribute('width')).toBe(false);
    expect(svg.getAttribute('viewBox')).toBe('0 0 400 200');
    expect(svg.classList.contains('legacy')).toBe(false);
    expect(out.querySelector('rect')?.getAttribute('fill')).toBe('url(#hatch-a)');
    expect(out.querySelector('text')?.getAttribute('transform')).toBe('rotate(-45 1 2)');
    expect(out.querySelector('figcaption')?.textContent).toBe('T');
  });

  it('strips scripts, handlers, links and embedded content', () => {
    const out = html(
      sanitizeViz(
        '<figure class="viz"><script>alert(1)</script>' +
          '<svg viewBox="0 0 10 10"><a href="https://example.com"><text onclick="x()">t</text></a>' +
          '<image href="https://example.com/a.png"/><foreignObject><div>x</div></foreignObject>' +
          '<use href="#a"/></svg>' +
          '<img src="x" onerror="alert(1)"><iframe src="https://example.com"></iframe>' +
          '<form action="/x"><input name="a"></form></figure>'
      )
    ).toLowerCase();
    for (const banned of [
      '<script',
      'alert(1)',
      'onclick',
      'onerror',
      'href',
      '<image',
      '<img',
      '<iframe',
      '<foreignobject',
      '<use',
      '<form',
      '<input',
    ]) {
      expect(out).not.toContain(banned);
    }
  });

  it('drops styles from our own figures, which carry none', () => {
    const out = html(
      sanitizeViz(
        '<figure class="viz viz-table"><style>*{display:none}</style>' +
          '<table class="viz-table" style="position:fixed"><tbody><tr><td>1</td></tr></tbody></table></figure>'
      )
    );
    expect(out).not.toContain('<style');
    expect(out).not.toContain('position');
    expect(out).toContain('<td>1</td>');
  });

  it("keeps Mermaid's stylesheet unless it reaches outside the figure", () => {
    const mermaid = (css: string) =>
      `<figure class="viz viz-mermaid"><svg id="merman" viewBox="0 0 10 10" style="max-width:10px"><style>${css}</style><g/></svg></figure>`;
    expect(html(sanitizeViz(mermaid('#merman .node{fill:var(--vz-paper)}')))).toContain(
      'var(--vz-paper)'
    );
    expect(html(sanitizeViz(mermaid('@import url(https://example.com/x.css);')))).not.toContain(
      'example.com'
    );
  });

  it('unwraps a pre-inline document and marks its svg legacy', () => {
    const out = sanitizeViz(
      '<!DOCTYPE html><html><head><title>T</title><style>body{background:#fafafa}</style></head>' +
        '<body data-content-w="300" data-content-h="100">' +
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 300 100" width="100%">' +
        '<defs><style>.node-box{fill:#ffffff}</style></defs><rect class="node-box"/></svg>' +
        '<script>window.parent.postMessage({}, "*")</script></body></html>'
    );
    const svg = out.querySelector('svg') as SVGElement;
    expect(svg.classList.contains('legacy')).toBe(true);
    expect(svg.hasAttribute('width')).toBe(false);
    const text = html(out);
    expect(text).not.toContain('<style');
    expect(text).not.toContain('postMessage');
    expect(text).not.toContain('#fafafa');
  });
});

describe('mountViz', () => {
  it('mounts into a shadow root and replaces the previous figure', () => {
    const host = document.createElement('div');
    document.body.append(host);
    const root = mountViz(
      host,
      '<figure class="viz"><figcaption class="viz-title">One</figcaption></figure>'
    );
    expect(host.shadowRoot).toBe(root);
    expect(root.querySelector('figcaption')?.textContent).toBe('One');
    // Nothing of the figure lands in the host's own (light) tree.
    expect(host.childNodes.length).toBe(0);
    mountViz(host, '<figure class="viz"><figcaption class="viz-title">Two</figcaption></figure>');
    expect(root.querySelectorAll('figcaption').length).toBe(1);
    expect(root.querySelector('figcaption')?.textContent).toBe('Two');
    host.remove();
  });
});
