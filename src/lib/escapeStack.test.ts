import { describe, expect, it, vi } from 'vitest';
import { hasEscapeLayer, registerEscapeLayer } from './escapeStack';
import { typedMatches } from '@/components/shared/ConfirmDialog';

const escape = () => {
  const e = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true });
  window.dispatchEvent(e);
  return e;
};

describe('escapeStack', () => {
  it('closes the topmost layer only, and keeps Escape from the window', () => {
    const below = vi.fn();
    const top = vi.fn();
    const windowHandler = vi.fn();
    window.addEventListener('keydown', windowHandler);
    const unBelow = registerEscapeLayer(below);
    const unTop = registerEscapeLayer(top);

    escape();
    expect(top).toHaveBeenCalledTimes(1);
    expect(below).not.toHaveBeenCalled();
    expect(windowHandler).not.toHaveBeenCalled();

    unTop();
    unBelow();
    expect(hasEscapeLayer()).toBe(false);
    escape();
    expect(windowHandler).toHaveBeenCalledTimes(1);
    window.removeEventListener('keydown', windowHandler);
  });
});

describe('typedMatches', () => {
  it('requires the exact phrase only when one is asked for', () => {
    expect(typedMatches(undefined, '')).toBe(true);
    expect(typedMatches('erase', '')).toBe(false);
    expect(typedMatches('erase', ' erase ')).toBe(true);
    expect(typedMatches('erase', 'Erase')).toBe(false);
  });
});
