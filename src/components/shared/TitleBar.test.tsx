import { describe, expect, it, vi } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import type { HubView } from '@/stores/routeStore';

const state = vi.hoisted(() => ({
  view: 'chat' as HubView,
  title: null as string | null,
  android: false,
}));

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({ minimize: vi.fn(), toggleMaximize: vi.fn(), close: vi.fn() }),
}));
vi.mock('@/lib/platform', () => ({ isAndroid: () => state.android }));
vi.mock('@/stores/routeStore', () => ({
  useRouteStore: (pick: (s: { view: HubView }) => unknown) => pick({ view: state.view }),
}));
vi.mock('@/stores/chatStore', () => ({
  useChatStore: (pick: (s: { title: string | null }) => unknown) => pick({ title: state.title }),
}));

import { TitleBar, titleFor } from './TitleBar';

function render(): string {
  return renderToStaticMarkup(<TitleBar actionsRef={() => {}} />);
}

describe('titleFor', () => {
  it('names each route, and the chat by its own title', () => {
    expect(titleFor('settings', 'x')).toBe('Settings');
    expect(titleFor('wizard', null)).toBe('Setup');
    expect(titleFor('sessions', null)).toBe('Saved chats');
    expect(titleFor('chat', 'Trip planning')).toBe('Trip planning');
  });

  it('says Kitty for a chat with no title yet', () => {
    expect(titleFor('chat', null)).toBe('Kitty');
    expect(titleFor('chat', '   ')).toBe('Kitty');
  });
});

describe('TitleBar', () => {
  it('draws a drag area, the title, an actions slot and the window buttons', () => {
    state.android = false;
    state.view = 'chat';
    state.title = 'Trip planning';
    const html = render();
    expect(html).toContain('data-tauri-drag-region');
    expect(html).toContain('Trip planning');
    expect(html).toContain('class="titlebar-actions-slot"');
    for (const label of ['Minimize', 'Maximize or restore', 'Close']) {
      expect(html).toContain(`aria-label="${label}"`);
    }
  });

  it('is not drawn on Android', () => {
    state.android = true;
    expect(render()).toBe('');
  });
});
