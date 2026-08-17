import { describe, it, expect } from 'vitest';
import { resolveThemeName, SYSTEM_THEME } from './theme';

/** D16: `system` is not a third stylesheet, it resolves to one of the two
    built-ins. Everything else — including a user's own CSS file — must pass
    through untouched, or dropping a theme named anything clever would break
    it. */
describe('resolveThemeName', () => {
  it('follows the OS preference when set to system', () => {
    expect(resolveThemeName(SYSTEM_THEME, true)).toBe('dark');
    expect(resolveThemeName(SYSTEM_THEME, false)).toBe('light');
  });

  it('leaves a pinned theme alone regardless of the OS preference', () => {
    for (const prefersDark of [true, false]) {
      expect(resolveThemeName('dark', prefersDark)).toBe('dark');
      expect(resolveThemeName('light', prefersDark)).toBe('light');
      expect(resolveThemeName('my-custom-theme', prefersDark)).toBe('my-custom-theme');
    }
  });

  it('treats "default" as an alias of "light" (pre-rename config/DOM values)', () => {
    for (const prefersDark of [true, false]) {
      expect(resolveThemeName('default', prefersDark)).toBe('light');
    }
  });

  /// The reserved name is compared exactly. A user theme file called
  /// `System.css` must not be hijacked by the OS-following behaviour.
  it('reserves only the exact name', () => {
    expect(resolveThemeName('System', true)).toBe('System');
    expect(resolveThemeName('systemic', true)).toBe('systemic');
  });
});
