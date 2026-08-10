import { describe, it, expect, beforeEach } from 'vitest';
import { parseRouteTarget, useRouteStore } from './routeStore';

beforeEach(() => {
  useRouteStore.setState({
    view: 'chat',
    settingsSection: null,
    settingsHighlight: null,
    wizardMode: 'setup',
  });
});

/** `route://goto` payloads cross the IPC boundary as untyped JSON, so this is
    the only place a Rust/TS disagreement about route names can be caught. */
describe('parseRouteTarget', () => {
  it('accepts the three routes the hub knows', () => {
    for (const view of ['chat', 'settings', 'wizard'] as const) {
      expect(parseRouteTarget({ view })?.view).toBe(view);
    }
  });

  it('carries the settings deep link through', () => {
    const t = parseRouteTarget({ view: 'settings', section: 'providers', highlight: 'abc' });
    expect(t?.opts.section).toBe('providers');
    expect(t?.opts.highlight).toBe('abc');
  });

  it('reads the wizard mode, defaulting away from a bogus one', () => {
    expect(parseRouteTarget({ view: 'wizard', mode: 'repair' })?.opts.mode).toBe('repair');
    expect(parseRouteTarget({ view: 'wizard', mode: 'setup' })?.opts.mode).toBe('setup');
    expect(parseRouteTarget({ view: 'wizard', mode: 'nonsense' })?.opts.mode).toBeUndefined();
  });

  /// An unknown view means Rust and this bundle disagree — most likely a
  /// stale window after an update. Staying put is recoverable; navigating to
  /// a route with no component renders a blank window with no way back.
  it('rejects anything it cannot render rather than navigating blind', () => {
    expect(parseRouteTarget({ view: 'sessions' })).toBeNull();
    expect(parseRouteTarget({ view: 'overlay' })).toBeNull();
    expect(parseRouteTarget({})).toBeNull();
    expect(parseRouteTarget(null)).toBeNull();
    expect(parseRouteTarget('settings')).toBeNull();
  });
});

describe('goto', () => {
  it('navigates and records the settings deep link', () => {
    useRouteStore.getState().goto('settings', { section: 'local_models', highlight: 'x' });
    const s = useRouteStore.getState();
    expect(s.view).toBe('settings');
    expect(s.settingsSection).toBe('local_models');
    expect(s.settingsHighlight).toBe('x');
  });

  /// "Open Settings" from the header must not silently reopen wherever a
  /// "Fix this" button last sent the user — that would look like the app
  /// remembering an error the user already dealt with.
  it('clears a stale deep link when Settings is opened plainly', () => {
    useRouteStore.getState().goto('settings', { section: 'providers' });
    useRouteStore.getState().goto('chat');
    useRouteStore.getState().goto('settings');
    expect(useRouteStore.getState().settingsSection).toBeNull();
  });

  /// Leaving Settings must not wipe the deep link, or coming back from a
  /// quick detour would land somewhere different than where you left.
  it('preserves the deep link while routed elsewhere', () => {
    useRouteStore.getState().goto('settings', { section: 'providers' });
    useRouteStore.getState().goto('chat');
    expect(useRouteStore.getState().settingsSection).toBe('providers');
  });

  it('remembers the wizard mode across a trip to another route', () => {
    useRouteStore.getState().goto('wizard', { mode: 'repair' });
    expect(useRouteStore.getState().wizardMode).toBe('repair');
    useRouteStore.getState().goto('chat');
    expect(useRouteStore.getState().wizardMode).toBe('repair');
  });
});
