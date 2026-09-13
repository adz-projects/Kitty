import { beforeEach, describe, expect, it } from 'vitest';
import { useMobileUiStore } from './mobileUiStore';

beforeEach(() => {
  useMobileUiStore.setState({ drawerOpen: false, revealedMessageId: null });
});

describe('mobileUiStore message actions', () => {
  it('reveals one message at a time', () => {
    const s = useMobileUiStore.getState();
    s.toggleRevealMessage('a');
    expect(useMobileUiStore.getState().revealedMessageId).toBe('a');
    s.toggleRevealMessage('b');
    expect(useMobileUiStore.getState().revealedMessageId).toBe('b');
  });

  it('hides a message when it is tapped again', () => {
    const s = useMobileUiStore.getState();
    s.toggleRevealMessage('a');
    s.toggleRevealMessage('a');
    expect(useMobileUiStore.getState().revealedMessageId).toBeNull();
  });
});
