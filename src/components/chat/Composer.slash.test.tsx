import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

// The composer reaches the backend only through `ipc` and Tauri's event
// listener; neither is needed to type into it.
vi.mock('@/lib/ipc', () => ({
  ipc: new Proxy({}, { get: () => vi.fn(() => Promise.resolve(null)) }),
  pickFiles: vi.fn(),
}));

import { Composer } from './Composer';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let host: HTMLDivElement;
let root: Root;
let onSend: ReturnType<typeof vi.fn>;

function textarea(): HTMLTextAreaElement {
  return host.querySelector('textarea') as HTMLTextAreaElement;
}

/** Type `value` the way a browser would, so React's onChange sees it. */
function type(value: string) {
  const ta = textarea();
  const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!;
  act(() => {
    setter.call(ta, value);
    ta.dispatchEvent(new Event('input', { bubbles: true }));
  });
}

function key(k: string) {
  act(() => {
    textarea().dispatchEvent(
      new KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true })
    );
  });
}

function options(): string[] {
  return Array.from(host.querySelectorAll('[role="option"] .slash-name')).map(
    (n) => n.textContent ?? ''
  );
}

beforeEach(() => {
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  onSend = vi.fn();
  act(() => {
    root.render(<Composer onSend={onSend} onStop={() => {}} disabled={false} />);
  });
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

describe('slash command suggestions', () => {
  it('appear for a leading slash and narrow as the command is typed', () => {
    expect(options()).toEqual([]);
    type('/');
    expect(options()).toEqual(['/deepresearch', '/critique', '/debate', '/tidy', '/compact']);
    type('/de');
    expect(options()).toEqual(['/deepresearch', '/debate']);
    type('hello');
    expect(options()).toEqual([]);
  });

  it('complete with Enter instead of sending, then get out of the way', () => {
    type('/deep');
    key('Enter');
    expect(onSend).not.toHaveBeenCalled();
    expect(textarea().value).toBe('/deepresearch ');
    expect(options()).toEqual([]);
  });

  it('move with the arrow keys and complete with Tab', () => {
    type('/de');
    key('ArrowDown');
    expect(host.querySelector('[aria-selected="true"] .slash-name')?.textContent).toBe('/debate');
    key('Tab');
    expect(textarea().value).toBe('/debate ');
  });

  it('close on Escape until the text changes', () => {
    type('/c');
    expect(options()).toHaveLength(2);
    act(() => {
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    });
    expect(options()).toEqual([]);
    type('/co');
    expect(options()).toEqual(['/compact']);
  });

  it('complete on a click', () => {
    type('/');
    const item = Array.from(host.querySelectorAll('[role="option"]')).find((li) =>
      li.textContent?.startsWith('/critique')
    ) as HTMLElement;
    act(() => item.click());
    expect(textarea().value).toBe('/critique ');
  });
});
