import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

vi.mock('@/lib/ipc', () => ({
  ipc: {
    listOutputs: vi.fn(() => Promise.resolve([])),
    openPath: vi.fn(() => Promise.resolve()),
    revealPath: vi.fn(() => Promise.resolve()),
    downloadFile: vi.fn(() => Promise.resolve()),
  },
}));
vi.mock('@/lib/platform', () => ({ isAndroid: () => false }));

import { ArtifactsPane } from './ArtifactsPane';

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let host: HTMLDivElement;
let root: Root;
let onClose: ReturnType<typeof vi.fn>;

function pointerDown(el: Element) {
  act(() => {
    el.dispatchEvent(new Event('pointerdown', { bubbles: true }));
  });
}

beforeEach(() => {
  host = document.createElement('div');
  document.body.appendChild(host);
  root = createRoot(host);
  onClose = vi.fn();
  act(() => {
    root.render(
      <div>
        <button data-artifacts-toggle>Artifacts</button>
        <div className="outside-target">Chat area</div>
        <div className="context-menu">
          <button>Open</button>
        </div>
        <ArtifactsPane onClose={onClose} />
      </div>
    );
  });
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

describe('ArtifactsPane click-away dismissal (desktop)', () => {
  it('closes when clicking outside the drawer', () => {
    const outside = host.querySelector('.outside-target') as HTMLElement;
    pointerDown(outside);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('stays open when clicking inside the drawer', () => {
    const inside = host.querySelector('.artifacts-head') as HTMLElement;
    pointerDown(inside);
    expect(onClose).not.toHaveBeenCalled();
  });

  it('ignores clicks on the Artifacts toggle button and context menus', () => {
    const toggle = host.querySelector('[data-artifacts-toggle]') as HTMLElement;
    const menuBtn = host.querySelector('.context-menu button') as HTMLElement;
    pointerDown(toggle);
    pointerDown(menuBtn);
    expect(onClose).not.toHaveBeenCalled();
  });
});
